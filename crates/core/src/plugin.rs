use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::value::Value;

/// Something a plugin wants the screen to react to: show `page` for `hold`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub page: String,
    pub hold: Duration,
    pub reason: String,
}

/// A data source. The scheduler calls `poll` every `interval` and writes the
/// values into the store under `name()/`. Plugins must not hold credentials in
/// values and must not block for longer than their interval.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// Key prefix in the store, also the plugin's display name.
    fn name(&self) -> &str;

    fn interval(&self) -> Duration;

    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>>;

    /// Long-lived event stream (camera rings, alerts). Default: none.
    fn events(&self) -> Option<BoxStream<'static, Event>> {
        None
    }
}
