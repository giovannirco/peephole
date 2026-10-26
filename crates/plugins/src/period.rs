use std::time::Duration;

/// A positive number of seconds from a setting; zero is a configuration error.
pub(crate) fn seconds(value: u64, setting: &str) -> anyhow::Result<Duration> {
    anyhow::ensure!(value > 0, "{setting} must be greater than 0");
    Ok(Duration::from_secs(value))
}

/// The request timeout from an optional `timeout_s`: unset means the default, capped at the interval.
pub(crate) fn timeout(timeout_s: Option<u64>, interval: Duration) -> anyhow::Result<Duration> {
    match timeout_s {
        Some(value) => seconds(value, "timeout_s"),
        None => Ok(crate::http::DEFAULT_TIMEOUT.min(interval)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_defaults_to_ten_seconds_capped_at_the_interval() {
        assert_eq!(timeout(None, Duration::from_secs(300)).unwrap(), Duration::from_secs(10));
        assert_eq!(timeout(None, Duration::from_secs(4)).unwrap(), Duration::from_secs(4));
        assert_eq!(timeout(Some(30), Duration::from_secs(300)).unwrap(), Duration::from_secs(30));
        assert!(timeout(Some(0), Duration::from_secs(300)).unwrap_err().to_string().contains("timeout_s"));
    }
}
