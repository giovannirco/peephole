use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;

use crate::plugin::{Event, Plugin};
use crate::value::{Store, Value};

/// Wakes a plugin's poll loop now; cheap to clone and share with HTTP handlers.
#[derive(Debug, Clone, Default)]
pub struct PollTriggers(Arc<HashMap<String, Arc<Notify>>>);

impl PollTriggers {
    /// Poll `name` as soon as its current poll (if any) ends; false when no such plugin runs.
    pub fn poll_now(&self, name: &str) -> bool {
        let Some(notify) = self.0.get(name) else { return false };
        notify.notify_one();
        true
    }
}

/// The running poll and event tasks of every plugin.
#[derive(Debug)]
pub struct Scheduler {
    handles: Vec<JoinHandle<()>>,
    triggers: PollTriggers,
}

impl Scheduler {
    pub fn triggers(&self) -> PollTriggers {
        self.triggers.clone()
    }

    pub fn poll_now(&self, name: &str) -> bool {
        self.triggers.poll_now(name)
    }

    pub fn abort(self) {
        for handle in self.handles {
            handle.abort();
        }
    }
}

/// Poll every plugin on its own interval into the store; forward plugin events.
/// A trigger polls a plugin at once without moving its interval.
pub fn run(plugins: Vec<Arc<dyn Plugin>>, store: Store, events: broadcast::Sender<Event>) -> Scheduler {
    let mut handles = Vec::new();
    let mut triggers = HashMap::new();
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
        let notify = Arc::new(Notify::new());
        triggers.insert(plugin.name().to_owned(), notify.clone());
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(plugin.interval());
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = notify.notified() => {}
                }
                poll_once(plugin.as_ref(), &store).await;
            }
        }));
    }
    Scheduler { handles, triggers: PollTriggers(Arc::new(triggers)) }
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
        let scheduler = run(vec![Arc::new(Counter(AtomicU32::new(0)))], store.clone(), tx);
        tokio::time::sleep(Duration::from_millis(120)).await;
        scheduler.abort();
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
        let scheduler = run(vec![plugin], store.clone(), tx);
        wait_for(|| store.get(&key).is_some()).await;
        scheduler.abort();
    }

    async fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    struct Slow(Arc<AtomicU32>);

    #[async_trait]
    impl Plugin for Slow {
        fn name(&self) -> &str {
            "slow"
        }
        fn interval(&self) -> Duration {
            Duration::from_secs(3600)
        }
        async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn poll_now_polls_a_plugin_immediately_and_only_that_plugin() {
        let polls = Arc::new(AtomicU32::new(0));
        let (tx, _rx) = broadcast::channel(8);
        let scheduler = run(vec![Arc::new(Slow(polls.clone()))], Store::new(), tx);
        wait_for(|| polls.load(Ordering::SeqCst) == 1).await;
        assert_eq!(polls.load(Ordering::SeqCst), 1, "the first tick polls at once");
        assert!(scheduler.poll_now("slow"));
        wait_for(|| polls.load(Ordering::SeqCst) == 2).await;
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert!(!scheduler.poll_now("nope"));
        assert!(scheduler.triggers().poll_now("slow"));
        wait_for(|| polls.load(Ordering::SeqCst) == 3).await;
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        scheduler.abort();
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
