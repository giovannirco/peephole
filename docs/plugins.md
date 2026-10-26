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

More rules learned while writing the direct plugins (v0.2):
- **Settings structs holding a secret do not derive `Debug`.** Fields named like a credential (`key`, `token`, `password`, `secret`, `bearer`, `auth`, `header`, `cookie`) are masked on the `/settings` panel; a `${VAR}` reference shows as its variable name.
- **Errors name the path, never the URL.** `crate::http::get` drops the URL from request errors (a token in a query string would otherwise land in `<plugin>/_error`). Do the same in your own request code (`reqwest::Error::without_url`).
- **Aggregates, not people.** Never emit hostnames, MACs, IPs, phone ids, e-mail addresses, torrent or job titles: counts, ratios and the names you configured yourself.
- **Least privilege is part of the plugin.** Document the smallest identity that works (Proxmox `PVEAuditor`, the Kubernetes ClusterRole in `docs/kubernetes-rbac.yaml`, a Technitium user with Dashboard view, a Forgejo token with `read:*` scopes) and refuse to need more.
- **Verify the upstream shape before writing the fixture.** Read the API source or spec and cite it in the module doc comment; several APIs differ from their folklore (CLIProxyAPI v8 has no usage endpoint, GitHub public push events carry no commit count, X uses `post.fields`).
- **Independent requests are `Partial` parts.** One failing part is logged and counted; the poll fails only when every part failed.

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
    - {key: rain_1h, pointer: /hourly, kind: series, element: /rain_mm, last: 24}
- kind: http_json
  name: lab
  url: https://orrery.example.com/api/insight/proxmox
  items:
    - pointer: /hosts
      key: /name
      prefix: host
      fields:
        - {key: cpu, pointer: /cpu}
        - {key: mem, pointer: /memoryUsed, over: /memoryTotal}
      count:
        - {key: up, pointer: /up, equals: true}
```

`http_json` field kinds: `number` (a boolean reads as 1/0), `text`, and `series` (a JSON array; `element` points into
each entry, e.g. `/1` for `[ts, value]` pairs; non-numbers become gaps; `last: N` keeps the tail). `over` divides two
pointers (0 when the divisor is 0), at the top level or in `items`. `items` reads one entry per array element as
`<prefix>/<label>/<key>`, plus `<prefix>_count` and one `<prefix>_<key>` per `count`; labels and keys have `/` and
spaces turned into `-`.

A recipe PR = the YAML page(s), the plugin entry, and a screenshot from the hub (`/pages/<name>.png?w=640&h=340`).
