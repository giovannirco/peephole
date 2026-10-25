use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Base of a mempool.space-compatible API (the part before `/api`).
    pub url: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
}

fn default_interval() -> u64 {
    60
}

const RING_HOURS: i64 = 24;

/// Last USD price seen in each of the last 24 hours.
#[derive(Default)]
struct PriceRing {
    slots: VecDeque<(i64, f64)>,
}

impl PriceRing {
    fn push(&mut self, hour: i64, price: f64) {
        match self.slots.back_mut() {
            Some((last, value)) if *last == hour => *value = price,
            _ => self.slots.push_back((hour, price)),
        }
        while self.slots.front().is_some_and(|(h, _)| *h <= hour - RING_HOURS) {
            self.slots.pop_front();
        }
    }

    fn series(&self, hour: i64) -> Vec<f64> {
        (hour - RING_HOURS + 1..=hour)
            .map(|h| self.slots.iter().find(|(slot, _)| *slot == h).map_or(f64::NAN, |(_, price)| *price))
            .collect()
    }
}

#[derive(Deserialize)]
struct Prices {
    #[serde(rename = "USD")]
    usd: f64,
    #[serde(rename = "EUR")]
    eur: f64,
    #[serde(rename = "BRL")]
    brl: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fees {
    fastest_fee: f64,
    half_hour_fee: f64,
    hour_fee: f64,
    economy_fee: f64,
    minimum_fee: f64,
}

#[derive(Deserialize)]
struct MempoolStats {
    count: f64,
    vsize: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Difficulty {
    progress_percent: f64,
    difficulty_change: f64,
    remaining_blocks: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hashrate {
    current_hashrate: f64,
}

type Values = Vec<(String, Value)>;

fn numbers(pairs: &[(&str, f64)]) -> Values {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), Value::Number(*v))).collect()
}

pub struct Mempool {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    base: reqwest::Url,
    ring: Mutex<PriceRing>,
}

impl Mempool {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(crate::http::DEFAULT_TIMEOUT.min(interval), &Default::default(), None, false)?,
            base: crate::http::base_url(&settings.url)?,
            ring: Mutex::new(PriceRing::default()),
        })
    }

    async fn json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> anyhow::Result<T> {
        Ok(crate::http::get(&self.client, crate::http::join(&self.base, path)).await?.json().await?)
    }

    async fn prices(&self) -> anyhow::Result<Values> {
        let p: Prices = self.json("api/v1/prices").await?;
        let hour = chrono::Utc::now().timestamp().div_euclid(3600);
        let series = {
            let mut ring = self.ring.lock().expect("price ring poisoned");
            ring.push(hour, p.usd);
            ring.series(hour)
        };
        let mut values = numbers(&[("price_usd", p.usd), ("price_eur", p.eur), ("price_brl", p.brl)]);
        values.push(("price_24h".into(), Value::Series(series)));
        Ok(values)
    }

    async fn tip(&self) -> anyhow::Result<Values> {
        let text = crate::http::get(&self.client, crate::http::join(&self.base, "api/blocks/tip/height"))
            .await?
            .text()
            .await?;
        let height: f64 =
            text.trim().parse().map_err(|_| anyhow::anyhow!("tip height {:?} is not a number", text.trim()))?;
        Ok(numbers(&[("tip_height", height)]))
    }

    async fn fees(&self) -> anyhow::Result<Values> {
        let f: Fees = self.json("api/v1/fees/recommended").await?;
        Ok(numbers(&[
            ("fee_fast", f.fastest_fee),
            ("fee_half_hour", f.half_hour_fee),
            ("fee_hour", f.hour_fee),
            ("fee_economy", f.economy_fee),
            ("fee_min", f.minimum_fee),
        ]))
    }

    async fn mempool(&self) -> anyhow::Result<Values> {
        let m: MempoolStats = self.json("api/mempool").await?;
        Ok(numbers(&[("mempool_count", m.count), ("mempool_vsize", m.vsize)]))
    }

    async fn difficulty(&self) -> anyhow::Result<Values> {
        let d: Difficulty = self.json("api/v1/difficulty-adjustment").await?;
        Ok(numbers(&[
            ("difficulty_progress", d.progress_percent / 100.0),
            ("difficulty_change", d.difficulty_change),
            ("remaining_blocks", d.remaining_blocks),
        ]))
    }

    async fn hashrate(&self) -> anyhow::Result<Values> {
        let h: Hashrate = self.json("api/v1/mining/hashrate/3d").await?;
        Ok(numbers(&[("hashrate", h.current_hashrate)]))
    }
}

#[async_trait]
impl Plugin for Mempool {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let (prices, tip, fees, mempool, difficulty, hashrate) =
            tokio::join!(self.prices(), self.tip(), self.fees(), self.mempool(), self.difficulty(), self.hashrate());
        let mut partial = crate::partial::Partial::new(&self.name, "endpoints");
        for (endpoint, result) in [
            ("prices", prices),
            ("tip_height", tip),
            ("fees", fees),
            ("mempool", mempool),
            ("difficulty", difficulty),
            ("hashrate", hashrate),
        ] {
            match result {
                Ok(values) => partial.ok(values),
                Err(e) => partial.fail(endpoint, &e),
            }
        }
        partial.finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn ring_overwrites_the_current_hour_and_fills_oldest_to_newest() {
        let mut ring = PriceRing::default();
        ring.push(100, 1.0);
        ring.push(100, 2.0);
        ring.push(101, 3.0);
        ring.push(103, 5.0);
        let s = ring.series(103);
        assert_eq!(s.len(), 24);
        assert!(s[..20].iter().all(|v| v.is_nan()));
        assert_eq!(s[20], 2.0);
        assert_eq!(s[21], 3.0);
        assert!(s[22].is_nan());
        assert_eq!(s[23], 5.0);
    }

    #[test]
    fn ring_keeps_only_the_last_day() {
        let mut ring = PriceRing::default();
        for hour in 0..30 {
            ring.push(hour, hour as f64);
        }
        let s = ring.series(29);
        assert_eq!(s, (6..30).map(|h| h as f64).collect::<Vec<_>>());
        assert!(ring.slots.len() <= 24);
    }

    async fn mount(server: &MockServer, route: &str, response: ResponseTemplate) {
        Mock::given(method("GET")).and(path(route)).respond_with(response).mount(server).await;
    }

    async fn full_server() -> MockServer {
        let server = MockServer::start().await;
        let ok = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
        mount(&server, "/api/v1/prices", ok(json!({"time": 1, "USD": 100000, "EUR": 90000, "BRL": 550000}))).await;
        mount(&server, "/api/blocks/tip/height", ResponseTemplate::new(200).set_body_string("920123")).await;
        mount(
            &server,
            "/api/v1/fees/recommended",
            ok(json!({"fastestFee": 12, "halfHourFee": 8, "hourFee": 5, "economyFee": 3, "minimumFee": 1})),
        )
        .await;
        mount(&server, "/api/mempool", ok(json!({"count": 4321, "vsize": 2100000, "total_fee": 1}))).await;
        mount(
            &server,
            "/api/v1/difficulty-adjustment",
            ok(json!({"progressPercent": 42.5, "difficultyChange": -1.25, "remainingBlocks": 1159})),
        )
        .await;
        mount(
            &server,
            "/api/v1/mining/hashrate/3d",
            ok(json!({"hashrates": [], "difficulty": [], "currentHashrate": 9.5e20, "currentDifficulty": 1.2e14})),
        )
        .await;
        server
    }

    fn plugin(server: &MockServer) -> Mempool {
        Mempool::new("btc", serde_yaml::from_str(&format!("url: {}", server.uri())).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn reads_every_endpoint() {
        let server = full_server().await;
        let got: BTreeMap<_, _> = plugin(&server).poll().await.unwrap().into_iter().collect();
        let n = |k: &str| got[k].as_f64().unwrap_or_else(|| panic!("{k} is not a number"));
        assert_eq!(n("price_usd"), 100000.0);
        assert_eq!(n("price_eur"), 90000.0);
        assert_eq!(n("price_brl"), 550000.0);
        assert_eq!(n("tip_height"), 920123.0);
        assert_eq!(n("fee_fast"), 12.0);
        assert_eq!(n("fee_half_hour"), 8.0);
        assert_eq!(n("fee_hour"), 5.0);
        assert_eq!(n("fee_economy"), 3.0);
        assert_eq!(n("fee_min"), 1.0);
        assert_eq!(n("mempool_count"), 4321.0);
        assert_eq!(n("mempool_vsize"), 2100000.0);
        assert_eq!(n("difficulty_progress"), 0.425);
        assert_eq!(n("difficulty_change"), -1.25);
        assert_eq!(n("remaining_blocks"), 1159.0);
        assert_eq!(n("hashrate"), 9.5e20);
        assert_eq!(n("_endpoints_ok"), 6.0);
        assert_eq!(n("_endpoints_failed"), 0.0);
    }

    #[tokio::test]
    async fn price_ring_fills_from_polls() {
        let server = full_server().await;
        let p = plugin(&server);
        p.poll().await.unwrap();
        let got: BTreeMap<_, _> = p.poll().await.unwrap().into_iter().collect();
        let ring = got["price_24h"].as_series().unwrap();
        assert_eq!(ring.len(), 24);
        assert_eq!(ring[23], 100000.0);
        assert!(ring[..22].iter().all(|v| v.is_nan()));
    }

    #[tokio::test]
    async fn a_failing_endpoint_leaves_the_others() {
        let server = MockServer::start().await;
        mount(&server, "/api/blocks/tip/height", ResponseTemplate::new(200).set_body_string("7\n")).await;
        let got: BTreeMap<_, _> = plugin(&server).poll().await.unwrap().into_iter().collect();
        assert_eq!(got["tip_height"], Value::Number(7.0));
        assert_eq!(got["_endpoints_ok"], Value::Number(1.0));
        assert_eq!(got["_endpoints_failed"], Value::Number(5.0));
        assert!(!got.contains_key("price_24h"));
    }

    #[tokio::test]
    async fn every_endpoint_failing_is_an_error() {
        let server = MockServer::start().await;
        let err = plugin(&server).poll().await.unwrap_err().to_string();
        assert!(err.contains("prices") && err.contains("404"), "{err}");
    }
}
