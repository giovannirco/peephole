use std::time::Duration;

/// A positive number of seconds from a setting; zero is a configuration error.
pub(crate) fn seconds(value: u64, setting: &str) -> anyhow::Result<Duration> {
    anyhow::ensure!(value > 0, "{setting} must be greater than 0");
    Ok(Duration::from_secs(value))
}
