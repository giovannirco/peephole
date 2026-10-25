use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_tz")]
    pub tz: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
}

fn default_tz() -> String {
    "UTC".into()
}
fn default_interval() -> u64 {
    1
}

pub struct Clock {
    name: String,
    tz: Tz,
    interval: Duration,
}

impl Clock {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let tz: Tz = settings.tz.parse().map_err(|_| anyhow::anyhow!("unknown time zone {:?}", settings.tz))?;
        Ok(Self { name: name.to_owned(), tz, interval: crate::period::seconds(settings.interval_s, "interval_s")? })
    }
}

fn values_at(now: DateTime<Utc>, tz: Tz) -> Vec<(String, Value)> {
    let local = now.with_timezone(&tz);
    vec![
        ("time".into(), Value::Text(local.format("%H:%M").to_string())),
        ("time_s".into(), Value::Text(local.format("%H:%M:%S").to_string())),
        ("date".into(), Value::Text(local.format("%a %-d %b").to_string())),
        ("weekday".into(), Value::Text(local.format("%A").to_string())),
        ("hour".into(), Value::Number(f64::from(local.hour()))),
        ("epoch".into(), Value::Number(now.timestamp() as f64)),
    ]
}

#[async_trait]
impl Plugin for Clock {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        Ok(values_at(Utc::now(), self.tz))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::TimeZone;

    use super::*;

    fn map(values: Vec<(String, Value)>) -> BTreeMap<String, Value> {
        values.into_iter().collect()
    }

    #[test]
    fn formats_the_instant_in_the_configured_zone() {
        let now = Utc.with_ymd_and_hms(2026, 10, 25, 14, 5, 9).unwrap();
        let got = map(values_at(now, chrono_tz::America::Sao_Paulo));
        assert_eq!(got["time"], Value::Text("11:05".into()));
        assert_eq!(got["time_s"], Value::Text("11:05:09".into()));
        assert_eq!(got["date"], Value::Text("Sun 25 Oct".into()));
        assert_eq!(got["weekday"], Value::Text("Sunday".into()));
        assert_eq!(got["hour"], Value::Number(11.0));
        assert_eq!(got["epoch"], Value::Number(now.timestamp() as f64));
    }

    #[test]
    fn date_rolls_over_with_the_zone() {
        let now = Utc.with_ymd_and_hms(2026, 10, 25, 1, 0, 0).unwrap();
        let got = map(values_at(now, chrono_tz::America::Sao_Paulo));
        assert_eq!(got["date"], Value::Text("Sat 24 Oct".into()));
        assert_eq!(got["hour"], Value::Number(22.0));
    }

    #[test]
    fn defaults_to_utc_every_second() {
        let clock = Clock::new("clock", serde_yaml::from_str("{}").unwrap()).unwrap();
        assert_eq!(clock.tz, Tz::UTC);
        assert_eq!(clock.interval(), Duration::from_secs(1));
        assert_eq!(clock.name(), "clock");
    }

    #[test]
    fn unknown_zone_is_rejected_at_build() {
        let err = Clock::new("c", serde_yaml::from_str("tz: Mars/Olympus").unwrap()).err().unwrap();
        assert!(err.to_string().contains("Mars/Olympus"), "{err}");
    }

    #[test]
    fn zero_interval_is_rejected() {
        assert!(Clock::new("c", serde_yaml::from_str("interval_s: 0").unwrap()).is_err());
    }
}
