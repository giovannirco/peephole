use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub values: BTreeMap<String, serde_yaml::Value>,
    #[serde(default = "default_jitter")]
    pub jitter: bool,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
}

fn default_jitter() -> bool {
    true
}
fn default_interval() -> u64 {
    5
}

const JITTER: f64 = 0.02;
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

enum Base {
    Number(f64),
    Text(String),
    Series(Vec<f64>),
}

impl Base {
    fn parse(key: &str, value: &serde_yaml::Value) -> anyhow::Result<Self> {
        match value {
            serde_yaml::Value::Number(n) => Ok(Base::Number(number(key, n)?)),
            serde_yaml::Value::String(s) => Ok(Base::Text(s.clone())),
            serde_yaml::Value::Sequence(items) => {
                anyhow::ensure!(!items.is_empty(), "demo value {key:?}: a series needs at least one point");
                let points = items
                    .iter()
                    .map(|item| match item {
                        serde_yaml::Value::Number(n) => number(key, n),
                        other => anyhow::bail!("demo value {key:?}: series points must be numbers, got {other:?}"),
                    })
                    .collect::<anyhow::Result<Vec<f64>>>()?;
                Ok(Base::Series(points))
            }
            other => {
                anyhow::bail!("demo value {key:?}: expected a number, a string or a list of numbers, got {other:?}")
            }
        }
    }
}

fn number(key: &str, n: &serde_yaml::Number) -> anyhow::Result<f64> {
    n.as_f64().ok_or_else(|| anyhow::anyhow!("demo value {key:?}: {n} is not representable as f64"))
}

/// Xorshift64: deterministic, good enough for wobbling demo numbers.
struct XorShift(u64);

impl XorShift {
    fn next_unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct State {
    rng: XorShift,
    step: usize,
    series: BTreeMap<String, Vec<f64>>,
}

impl State {
    fn wobble(&mut self, jitter: bool, value: f64) -> f64 {
        if !jitter {
            return value;
        }
        value * (1.0 + JITTER * (2.0 * self.rng.next_unit() - 1.0))
    }
}

pub struct Demo {
    name: String,
    interval: Duration,
    jitter: bool,
    base: BTreeMap<String, Base>,
    state: Mutex<State>,
}

impl Demo {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let base = settings
            .values
            .iter()
            .map(|(k, v)| Ok((k.clone(), Base::parse(k, v)?)))
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
        let series = base
            .iter()
            .filter_map(|(k, b)| match b {
                Base::Series(points) => Some((k.clone(), points.clone())),
                _ => None,
            })
            .collect();
        Ok(Self {
            name: name.to_owned(),
            interval: crate::period::seconds(settings.interval_s, "interval_s")?,
            jitter: settings.jitter,
            base,
            state: Mutex::new(State { rng: XorShift(SEED), step: 0, series }),
        })
    }
}

#[async_trait]
impl Plugin for Demo {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let mut state = self.state.lock().expect("demo state poisoned");
        let step = state.step;
        state.step += 1;
        let mut out = Vec::with_capacity(self.base.len());
        for (key, base) in &self.base {
            let value = match base {
                Base::Number(n) => Value::Number(state.wobble(self.jitter, *n)),
                Base::Text(t) => Value::Text(t.clone()),
                Base::Series(points) => {
                    let fresh = state.wobble(self.jitter, points[step % points.len()]);
                    let current = state.series.get_mut(key).expect("series state exists for every series");
                    current.rotate_left(1);
                    *current.last_mut().expect("series are never empty") = fresh;
                    Value::Series(current.clone())
                }
            };
            out.push((key.clone(), value));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo(yaml: &str) -> Demo {
        Demo::new("demo", serde_yaml::from_str(yaml).unwrap()).unwrap()
    }

    async fn poll(d: &Demo) -> BTreeMap<String, Value> {
        d.poll().await.unwrap().into_iter().collect()
    }

    const VALUES: &str = "values: {cpu: 50, label: hello, load: [1, 2, 3, 4]}";

    #[tokio::test]
    async fn same_settings_give_the_same_sequence() {
        let (a, b) = (demo(VALUES), demo(VALUES));
        for _ in 0..5 {
            assert_eq!(poll(&a).await, poll(&b).await);
        }
    }

    #[tokio::test]
    async fn numbers_wobble_within_two_percent_and_text_passes_through() {
        let d = demo(VALUES);
        let mut seen = Vec::new();
        for _ in 0..20 {
            let got = poll(&d).await;
            let cpu = got["cpu"].as_f64().unwrap();
            assert!((49.0..=51.0).contains(&cpu), "{cpu}");
            seen.push(cpu);
            assert_eq!(got["label"], Value::Text("hello".into()));
        }
        assert!(seen.iter().any(|v| *v != seen[0]), "{seen:?}");
    }

    #[tokio::test]
    async fn series_rotate_one_step_with_a_fresh_last_point() {
        let d = demo(VALUES);
        let mut prev = poll(&d).await["load"].as_series().unwrap().to_vec();
        for _ in 0..6 {
            let next = poll(&d).await["load"].as_series().unwrap().to_vec();
            assert_eq!(next.len(), 4);
            assert_eq!(next[..3], prev[1..]);
            prev = next;
        }
        assert!(prev.iter().all(|v| (0.98..=4.08).contains(v)), "{prev:?}");
    }

    #[tokio::test]
    async fn without_jitter_numbers_hold_and_series_rotate_exactly() {
        let d = demo("{jitter: false, values: {cpu: 50, load: [1, 2, 3]}}");
        let first = poll(&d).await;
        assert_eq!(first["cpu"], Value::Number(50.0));
        assert_eq!(first["load"], Value::Series(vec![2.0, 3.0, 1.0]));
        let second = poll(&d).await;
        assert_eq!(second["cpu"], Value::Number(50.0));
        assert_eq!(second["load"], Value::Series(vec![3.0, 1.0, 2.0]));
    }

    #[test]
    fn defaults_to_five_seconds() {
        assert_eq!(demo(VALUES).interval(), Duration::from_secs(5));
    }

    #[test]
    fn unsupported_values_are_rejected_with_their_key() {
        for yaml in ["values: {bad: {a: 1}}", "values: {bad: [1, x]}", "values: {bad: []}"] {
            let err = Demo::new("demo", serde_yaml::from_str(yaml).unwrap()).err().unwrap();
            assert!(err.to_string().contains("bad"), "{yaml}: {err}");
        }
    }
}
