use std::sync::Arc;

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
            let name = plugin.name().to_owned();
            let mut tick = tokio::time::interval(plugin.interval());
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match tokio::time::timeout(plugin.interval(), plugin.poll()).await {
                    Ok(Ok(values)) => {
                        let n = values.len();
                        store.set_many(&name, values);
                        store.set(format!("{name}/_ok"), Value::Number(1.0));
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
            }
        }));
    }
    handles
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
}
