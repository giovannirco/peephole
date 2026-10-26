use serde::Serialize;
use serde_json::{Map, Value as Json, json};
use serde_yaml::Value as Yaml;

use super::{ConfigError, replace_refs};

const SECRET_WORDS: &[&str] =
    &["key", "token", "password", "secret", "pass", "bearer", "credential", "auth", "header", "cookie"];
const PLACEHOLDER: &str = "__PEEPHOLE_ENV_";
const HIDDEN: &str = "••••";

/// One plugin's settings as written in the config, safe to show anyone.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RedactedPlugin {
    pub name: String,
    pub kind: String,
    pub settings: Json,
}

struct EnvRef {
    name: String,
    default: Option<String>,
    set: bool,
}

/// The plugins of the unexpanded config `raw_text` with every `${VAR}` shown as
/// `{"env", "set", "default"?}`, values under secret-looking keys as `{"secret": true}`
/// and URLs without userinfo or secret query values. `lookup` only answers whether a variable is set.
pub fn redact(raw_text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Vec<RedactedPlugin>, ConfigError> {
    let mut refs = Vec::new();
    let marked = replace_refs(raw_text, |name, default| {
        refs.push(EnvRef { name: name.to_owned(), default: default.map(str::to_owned), set: lookup(name).is_some() });
        Ok(format!("{PLACEHOLDER}{}__", refs.len() - 1))
    })?;
    let root: Yaml = serde_yaml::from_str(&marked)?;
    let Some(Yaml::Sequence(plugins)) = root.get("plugins") else { return Ok(Vec::new()) };
    Ok(plugins.iter().filter_map(Yaml::as_mapping).map(|entry| plugin(entry, &refs)).collect())
}

fn plugin(entry: &serde_yaml::Mapping, refs: &[EnvRef]) -> RedactedPlugin {
    let label = |key: &str| entry.get(key).and_then(Yaml::as_str).map(|s| unmark(s, refs));
    let kind = label("kind").unwrap_or_default();
    let name = label("name").unwrap_or_else(|| kind.clone());
    let settings = entry
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), Some("kind" | "name")))
        .map(|(k, v)| {
            let key = key_text(k, refs);
            let shown = show(v, refs, is_secret_key(&key));
            (key, shown)
        })
        .collect::<Map<String, Json>>();
    RedactedPlugin { name, kind, settings: Json::Object(settings) }
}

fn show(value: &Yaml, refs: &[EnvRef], secret: bool) -> Json {
    match value {
        Yaml::Null => Json::Null,
        Yaml::String(s) => show_string(s, refs, secret),
        Yaml::Bool(_) | Yaml::Number(_) if secret => json!({ "secret": true }),
        Yaml::Bool(b) => json!(b),
        Yaml::Number(n) => serde_json::to_value(n).unwrap_or(Json::Null),
        Yaml::Sequence(items) => Json::Array(items.iter().map(|v| show(v, refs, secret)).collect()),
        Yaml::Mapping(map) => Json::Object(
            map.iter()
                .map(|(k, v)| {
                    let key = key_text(k, refs);
                    let shown = show(v, refs, secret || is_secret_key(&key));
                    (key, shown)
                })
                .collect(),
        ),
        Yaml::Tagged(tagged) => show(&tagged.value, refs, secret),
    }
}

fn show_string(text: &str, refs: &[EnvRef], secret: bool) -> Json {
    let used: Vec<Json> =
        placeholders(text).into_iter().filter_map(|i| refs.get(i)).map(|r| show_ref(r, secret)).collect();
    match used.as_slice() {
        [] if secret || looks_like_credential(text) => json!({ "secret": true }),
        [] => json!(redact_url(text)),
        [one] => one.clone(),
        _ => json!({ "envs": used }),
    }
}

fn show_ref(r: &EnvRef, secret: bool) -> Json {
    let mut out = json!({ "env": r.name, "set": r.set });
    match (&r.default, secret) {
        (_, true) => out["secret"] = json!(true),
        (Some(d), false) if looks_like_credential(d) => out["default"] = json!(HIDDEN),
        (Some(d), false) => out["default"] = json!(redact_url(d)),
        (None, false) => {}
    }
    out
}

/// Indices of the placeholders in `text`, in order.
fn placeholders(text: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(PLACEHOLDER) {
        let after = &rest[start + PLACEHOLDER.len()..];
        let digits = after.find("__").unwrap_or(after.len());
        if let Ok(i) = after[..digits].parse() {
            found.push(i);
        }
        rest = &after[digits..];
    }
    found
}

/// `text` with each placeholder written back as `${NAME}`.
fn unmark(text: &str, refs: &[EnvRef]) -> String {
    refs.iter()
        .enumerate()
        .fold(text.to_owned(), |acc, (i, r)| acc.replace(&format!("{PLACEHOLDER}{i}__"), &format!("${{{}}}", r.name)))
}

fn key_text(key: &Yaml, refs: &[EnvRef]) -> String {
    match key {
        Yaml::String(s) => unmark(s, refs),
        Yaml::Number(n) => n.to_string(),
        Yaml::Bool(b) => b.to_string(),
        _ => "?".to_owned(),
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_lowercase();
    SECRET_WORDS.iter().any(|w| key.contains(w))
}

fn looks_like_credential(text: &str) -> bool {
    let lower = text.trim_start().to_lowercase();
    lower.starts_with("bearer ") || lower.starts_with("basic ")
}

/// A URL without its userinfo and with the values of secret-looking query parameters hidden;
/// any other text unchanged.
pub fn redact_url(text: &str) -> String {
    let Some((scheme, rest)) = text.split_once("://") else { return text.to_owned() };
    let valid_scheme = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c));
    if !valid_scheme {
        return text.to_owned();
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{}", redact_query(tail))
}

fn redact_query(tail: &str) -> String {
    let Some((path, query)) = tail.split_once('?') else { return tail.to_owned() };
    let (query, fragment) = query.split_once('#').map_or((query, None), |(q, f)| (q, Some(f)));
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_secret_key(k) => format!("{k}={HIDDEN}"),
            _ => pair.to_owned(),
        })
        .collect();
    let fragment = fragment.map(|f| format!("#{f}")).unwrap_or_default();
    format!("{path}?{}{fragment}", pairs.join("&"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::Config;

    fn env(k: &str) -> Option<String> {
        match k {
            "PROM_URL" => Some("http://u:p@prom:9090".into()),
            "TOK" => Some("SENTINEL-TOKEN".into()),
            _ => None,
        }
    }

    fn settings_of(yaml: &str) -> serde_json::Value {
        let plugins = redact(yaml, env).unwrap();
        assert_eq!(plugins.len(), 1, "{plugins:?}");
        plugins[0].settings.clone()
    }

    #[test]
    fn an_env_reference_shows_its_name_whether_it_is_set_and_its_default() {
        let s = settings_of(
            "plugins:\n  - kind: prometheus\n    url: ${PROM_URL}\n    tz: ${TZ:-UTC}\n    other: ${NOPE}\n",
        );
        assert_eq!(s["url"], json!({"env": "PROM_URL", "set": true}));
        assert_eq!(s["tz"], json!({"env": "TZ", "set": false, "default": "UTC"}));
        assert_eq!(s["other"], json!({"env": "NOPE", "set": false}));
    }

    #[test]
    fn two_variables_in_one_string_are_both_listed() {
        let s = settings_of("plugins: [{kind: x, url: \"${PROM_URL}/api?x=${TZ:-1}\"}]\n");
        assert_eq!(
            s["url"],
            json!({"envs": [{"env": "PROM_URL", "set": true}, {"env": "TZ", "set": false, "default": "1"}]})
        );
    }

    #[test]
    fn unquoted_references_inside_flow_mappings_parse() {
        let s = settings_of("plugins: [{kind: x, url: ${PROM_URL}, n: 3}]\n");
        assert_eq!(s, json!({"url": {"env": "PROM_URL", "set": true}, "n": 3}));
    }

    #[test]
    fn values_under_secret_keys_are_hidden_at_any_depth() {
        let yaml = r#"
plugins:
  - kind: x
    api_key: abc
    password: 123456
    bearer_token: ${TOK}
    hosts: [{name: a, token_secret: zzz, port: 22}]
    nested: {deeper: {Credentials: [one, two]}}
    headers: {Authorization: Bearer abc, X-Other: plain}
    pass_through: true
"#;
        let s = settings_of(yaml);
        assert_eq!(s["api_key"], json!({"secret": true}));
        assert_eq!(s["password"], json!({"secret": true}));
        assert_eq!(s["bearer_token"], json!({"env": "TOK", "set": true, "secret": true}));
        assert_eq!(s["hosts"], json!([{"name": "a", "token_secret": {"secret": true}, "port": 22}]));
        assert_eq!(s["nested"], json!({"deeper": {"Credentials": [{"secret": true}, {"secret": true}]}}));
        assert_eq!(s["headers"], json!({"Authorization": {"secret": true}, "X-Other": {"secret": true}}));
        assert_eq!(s["pass_through"], json!({"secret": true}));
    }

    #[test]
    fn a_default_under_a_secret_key_is_not_shown() {
        let s = settings_of("plugins: [{kind: x, token: \"${NOPE:-fallback-secret}\"}]\n");
        assert_eq!(s["token"], json!({"env": "NOPE", "set": false, "secret": true}));
    }

    #[test]
    fn bearer_and_basic_values_are_hidden_under_any_key() {
        let s = settings_of("plugins: [{kind: x, h: Bearer abc, b: basic Zm9v}]\n");
        assert_eq!(s, json!({"h": {"secret": true}, "b": {"secret": true}}));
    }

    #[test]
    fn urls_lose_their_userinfo_and_secret_query_values() {
        let yaml = "plugins: [{kind: x, a: \"https://u:p@host:8443/p?q=1\", b: \"http://host/x?api_key=k1&page=2#f\", \
                    c: \"https://host\", d: \"${NOPE:-https://me:pw@h/}\"}]\n";
        let s = settings_of(yaml);
        assert_eq!(s["a"], "https://host:8443/p?q=1");
        assert_eq!(s["b"], "http://host/x?api_key=••••&page=2#f");
        assert_eq!(s["c"], "https://host");
        assert_eq!(s["d"], json!({"env": "NOPE", "set": false, "default": "https://h/"}));
    }

    #[test]
    fn text_that_only_looks_like_a_placeholder_is_kept() {
        let s = settings_of("plugins: [{kind: x, a: __PEEPHOLE_ENV_99__}]\n");
        assert_eq!(s["a"], "__PEEPHOLE_ENV_99__");
    }

    #[test]
    fn plain_strings_numbers_and_lists_are_kept() {
        let s = settings_of("plugins: [{kind: clock, tz: Europe/Lisbon, interval_s: 5, on: true, list: [a, 1]}]\n");
        assert_eq!(s, json!({"tz": "Europe/Lisbon", "interval_s": 5, "on": true, "list": ["a", 1]}));
    }

    #[test]
    fn name_and_kind_are_kept_out_of_the_settings() {
        let plugins = redact("plugins: [{kind: demo, name: d, values: {a: 1}}, {kind: clock}]\n", env).unwrap();
        assert_eq!((plugins[0].name.as_str(), plugins[0].kind.as_str()), ("d", "demo"));
        assert_eq!(plugins[0].settings, json!({"values": {"a": 1}}));
        assert_eq!((plugins[1].name.as_str(), plugins[1].kind.as_str()), ("clock", "clock"));
        assert_eq!(plugins[1].settings, json!({}));
    }

    #[test]
    fn config_parse_keeps_the_redacted_view_with_the_expanded_names() {
        let cfg = Config::parse("plugins: [{kind: x, name: \"${NOPE:-p1}\", token: \"${TOK}\"}]\n", env).unwrap();
        assert_eq!(cfg.redacted.len(), 1);
        assert_eq!(cfg.redacted[0].name, "p1");
        assert!(!serde_json::to_string(&cfg.redacted).unwrap().contains("SENTINEL"));
    }
}
