use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::page::Page;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("environment variable {0} is not set (referenced as ${{{0}}} in the config)")]
    MissingEnv(String),
    #[error("config: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubConfig {
    #[serde(default = "default_title")]
    pub title: String,
    /// Seconds a device stays on a page before auto-cycling; 0 = no cycling.
    #[serde(default = "default_cycle")]
    pub cycle_s: u64,
    /// Seconds after which a value is drawn as stale.
    #[serde(default = "default_stale")]
    pub stale_s: u64,
}

fn default_title() -> String {
    "Peephole".into()
}
fn default_cycle() -> u64 {
    10
}
fn default_stale() -> u64 {
    300
}

impl Default for HubConfig {
    fn default() -> Self {
        Self { title: default_title(), cycle_s: default_cycle(), stale_s: default_stale() }
    }
}

/// One plugin entry: `kind` picks the implementation; the rest is the plugin's own schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginEntry {
    pub kind: String,
    /// Store prefix; defaults to `kind`.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(flatten)]
    pub settings: serde_yaml::Mapping,
}

impl PluginEntry {
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.kind)
    }
    /// Deserialize this entry's settings into a plugin's own config type.
    pub fn settings<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_yaml::Error> {
        serde_yaml::from_value(serde_yaml::Value::Mapping(self.settings.clone()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Config {
    #[serde(default)]
    pub hub: HubConfig,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
    #[serde(default)]
    pub pages: Vec<Page>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Io { path: path.display().to_string(), source })?;
        Self::parse(&text, |k| std::env::var(k).ok())
    }

    /// Parse YAML after replacing `${VAR}` / `${VAR:-default}` with values from `lookup`.
    pub fn parse(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let expanded = expand_env(text, lookup)?;
        let cfg: Config = serde_yaml::from_str(&expanded)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut seen = HashMap::new();
        for p in &self.pages {
            if seen.insert(p.name.clone(), ()).is_some() {
                return Err(ConfigError::Invalid(format!("page {:?} is defined twice", p.name)));
            }
        }
        let mut names = HashMap::new();
        for p in &self.plugins {
            if names.insert(p.name().to_owned(), ()).is_some() {
                return Err(ConfigError::Invalid(format!("plugin name {:?} is used twice", p.name())));
            }
        }
        Ok(())
    }

    pub fn page(&self, name: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.name == name)
    }
}

/// Replace `${VAR}` and `${VAR:-default}`; a missing variable without default is an error.
pub fn expand_env(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(ConfigError::Invalid("unterminated ${".into()));
        };
        let inner = &after[..end];
        let (name, default) = match inner.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (inner, None),
        };
        match lookup(name) {
            Some(v) => out.push_str(&v),
            None => match default {
                Some(d) => out.push_str(d),
                None => return Err(ConfigError::MissingEnv(name.to_owned())),
            },
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(k: &str) -> Option<String> {
        match k {
            "PROM" => Some("http://prom:9090".into()),
            _ => None,
        }
    }

    #[test]
    fn expands_variables_and_defaults() {
        assert_eq!(expand_env("a ${PROM} b", env).unwrap(), "a http://prom:9090 b");
        assert_eq!(expand_env("${X:-dflt}", env).unwrap(), "dflt");
        assert_eq!(expand_env("${PROM:-x}", env).unwrap(), "http://prom:9090");
        assert_eq!(expand_env("plain", env).unwrap(), "plain");
    }

    #[test]
    fn missing_variable_fails_fast_with_its_name() {
        let err = expand_env("${MISSING}", env).unwrap_err();
        assert!(matches!(err, ConfigError::MissingEnv(ref n) if n == "MISSING"), "{err}");
    }

    #[test]
    fn parses_plugins_with_their_own_settings_and_pages() {
        let yaml = r#"
hub: {title: Homelab, cycle_s: 15}
plugins:
  - kind: prometheus
    name: prom
    url: ${PROM}
    queries: [{key: cpu, expr: "up"}]
  - kind: clock
pages:
  - name: Overview
    cards: []
"#;
        let cfg = Config::parse(yaml, env).unwrap();
        assert_eq!(cfg.hub.cycle_s, 15);
        assert_eq!(cfg.hub.stale_s, 300);
        assert_eq!(cfg.plugins[0].name(), "prom");
        assert_eq!(cfg.plugins[1].name(), "clock");
        #[derive(Deserialize)]
        struct S {
            url: String,
        }
        let s: S = cfg.plugins[0].settings().unwrap();
        assert_eq!(s.url, "http://prom:9090");
        assert!(cfg.page("Overview").is_some());
    }

    #[test]
    fn duplicate_page_names_are_rejected() {
        let yaml = "pages: [{name: A}, {name: A}]";
        assert!(matches!(Config::parse(yaml, env), Err(ConfigError::Invalid(_))));
    }
}
