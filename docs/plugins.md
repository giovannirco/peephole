# Writing a plugin

A plugin is a Rust type that implements `peephole_core::Plugin`:

```rust
#[async_trait]
impl Plugin for Weather {
    fn name(&self) -> &str { &self.name }
    fn interval(&self) -> Duration { Duration::from_secs(600) }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let w: Api = self.client.get(&self.url).send().await?.json().await?;
        Ok(vec![("temp".into(), Value::Number(w.temp)), ("sky".into(), Value::Text(w.summary))])
    }
}
```

The scheduler calls `poll` every `interval`, prefixes the keys with the plugin's `name` and writes them into the store (`weather/temp`). It also writes `weather/_ok` (1 or 0) and `weather/_error`. Pages bind to the keys.

Rules of the house:
- **Never put a credential in a value.** The store is visible on `/api/values`.
- **Fail loudly, partially.** If one of several sub-requests fails, return the rest and let the missing keys go stale; return `Err` only when nothing worked. The renderer draws stale values grey with their age.
- **Keys are stable identifiers**, not labels: `camera/front-door/online`, not `Front Door`. Labels for the screen come from the page config (`items[].name`).
- **Series** (`Value::Series`) are plain `Vec<f64>` oldest first, NaN for gaps.
- **Images** (`Value::Image`) are RGBA8 already sized for a screen (keep them under 320×240).
- **Events** (`fn events()`) are for things a human should see now: a doorbell, an alert. Return a stream of `Event { page, hold, reason }`; every device shows that page for `hold`.

Register the kind in `crates/plugins/src/lib.rs` (`build` and `kinds`), add a settings struct that `serde` can read from the plugin's YAML entry, tests with `wiremock` fixtures (use `example.invalid` hosts), and a row in the README table.

## Recipes without code

Most sources are already covered by two generic plugins:

```yaml
- kind: prometheus
  name: p
  url: ${PROMETHEUS_URL}
  queries:
    - {key: temp, expr: 'avg(sensor_temperature_celsius)'}
    - {key: temp_1h, expr: 'avg(sensor_temperature_celsius)', range: {span_s: 3600, step_s: 120}}
    - {key: up, expr: 'up', label: job}
- kind: http_json
  name: weather
  url: https://api.example.com/now
  fields:
    - {key: temp, pointer: /current/temp_c}
    - {key: sky, pointer: /current/condition/text, kind: text}
```

A recipe PR = the YAML page(s), the plugin entry, and a screenshot from the hub (`/pages/<name>.png?w=640&h=340`).
