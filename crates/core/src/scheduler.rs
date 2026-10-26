use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::plugin::{Event, Plugin};
use crate::value::{Store, Value};

/// Poll every plugin on its own interval into the store; forward plugin events.
/// Returns the task handles so the hub can abort them on shutdown.
pub fn run(plugins: Vec<Arc<dyn Plugin>>, store: Store, events: broadcast::Sender<Event>) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    for plugin in plugins {
        if let Some(mut stream) = plugin.events() {
            let tx = events.clone();
            let name = plugin.name().to_owned();
            handles.push(tokio::spawn(async move {
                while let Some(ev) = stream.next().await {
                    tracing::info!(plugin = %name, page = %ev.page, reason = %ev.reason, "event");
                    let _ = tx.send(ev);
                }
            }));
        }
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(plugin.interval());
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                poll_once(plugin.as_ref(), &store).await;
            }
        }));
    }
    handles
}

/// One poll: write the values, `_ok`, `_error` on failure, `_polled_at` always and `_values` on success.
async fn poll_once(plugin: &dyn Plugin, store: &Store) {
    let name = plugin.name();
    match tokio::time::timeout(plugin.interval(), plugin.poll()).await {
        Ok(Ok(values)) => {
            let n = values.len();
            store.set_many(name, values);
            store.set(format!("{name}/_ok"), Value::Number(1.0));
            store.set(format!("{name}/_values"), Value::Number(n as f64));
            tracing::debug!(plugin = %name, values = n, "polled");
        }
        Ok(Err(e)) => {
            store.set(format!("{name}/_ok"), Value::Number(0.0));
            store.set(format!("{name}/_error"), Value::Text(e.to_string()));
            tracing::warn!(plugin = %name, error = %e, "poll failed");
        }
        Err(_) => {
            store.set(format!("{name}/_ok"), Value::Number(0.0));
            store.set(format!("{name}/_error"), Value::Text("timed out".into()));
            tracing::warn!(plugin = %name, "poll timed out");
        }
    }
    store.set(format!("{name}/_polled_at"), Value::Number(Utc::now().timestamp_millis() as f64 / 1000.0));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;

    struct Counter(AtomicU32);

    #[async_trait]
    impl Plugin for Counter {
        fn name(&self) -> &str {
            "ctr"
        }
        fn interval(&self) -> Duration {
            Duration::from_millis(20)
        }
        async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                anyhow::bail!("boom");
            }
            Ok(vec![("n".into(), Value::Number(n as f64))])
        }
    }

    #[tokio::test]
    async fn polls_on_interval_and_records_errors() {
        let store = Store::new();
        let (tx, _rx) = broadcast::channel(8);
        let handles = run(vec![Arc::new(Counter(AtomicU32::new(0)))], store.clone(), tx);
        tokio::time::sleep(Duration::from_millis(120)).await;
        for h in handles {
            h.abort();
        }
        assert!(store.num("ctr/n").unwrap() >= 2.0);
        assert_eq!(store.num("ctr/_ok"), Some(1.0));
        assert!(store.text("ctr/_error").is_some());
    }

    struct Failing;

    #[async_trait]
    impl Plugin for Failing {
        fn name(&self) -> &str {
            "bad"
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(60)
        }
        async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
            anyhow::bail!("down")
        }
    }

    async fn first_poll(plugin: Arc<dyn Plugin>, store: &Store) {
        let (tx, _rx) = broadcast::channel(8);
        let key = format!("{}/_polled_at", plugin.name());
        let handles = run(vec![plugin], store.clone(), tx);
        for _ in 0..100 {
            if store.get(&key).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for h in handles {
            h.abort();
        }
    }

    #[tokio::test]
    async fn a_successful_poll_records_when_it_ran_and_how_many_values_it_wrote() {
        let store = Store::new();
        let before = chrono::Utc::now().timestamp() as f64;
        first_poll(Arc::new(Counter(AtomicU32::new(0))), &store).await;
        assert_eq!(store.num("ctr/_values"), Some(1.0));
        let at = store.num("ctr/_polled_at").expect("polled_at");
        assert!(at >= before && at <= (chrono::Utc::now().timestamp() + 1) as f64, "{at}");
    }

    #[tokio::test]
    async fn a_failed_poll_records_when_it_ran_but_no_value_count() {
        let store = Store::new();
        first_poll(Arc::new(Failing), &store).await;
        assert!(store.num("bad/_polled_at").is_some());
        assert_eq!(store.num("bad/_values"), None);
        assert_eq!(store.num("bad/_ok"), Some(0.0));
    }
}
