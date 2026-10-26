use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use peephole_core::Config;

const DEMO_CONFIG: &str = include_str!("../../../examples/demo.yaml");
const DEMO_KINDS: &[&str] = &["clock", "demo"];
const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
/// Every variable the hub reads, in the order the settings panel lists them.
pub const VARIABLES: &[&str] = &[
    "PEEPHOLE_CONFIG",
    "PEEPHOLE_LISTEN",
    "PEEPHOLE_DEMO",
    "PEEPHOLE_STATE",
    "PEEPHOLE_FIRMWARE_DIR",
    "PEEPHOLE_PUBLIC_URL",
    "PEEPHOLE_API_TOKEN",
];

/// Everything the hub reads from the environment, resolved once at startup.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub config: Option<PathBuf>,
    pub listen: SocketAddr,
    pub demo: bool,
    pub state_file: Option<PathBuf>,
    pub firmware_dir: Option<PathBuf>,
    pub public_url: Option<String>,
    pub api_token: Option<ApiToken>,
    /// The variables of [`VARIABLES`] that were set (non-empty).
    pub set_variables: Vec<&'static str>,
}

/// The bearer token every `POST` must carry when set. `Debug` never prints it.
#[derive(Clone, PartialEq)]
pub struct ApiToken(String);

impl ApiToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Compare without returning at the first differing byte.
    pub fn matches(&self, presented: &str) -> bool {
        let (want, got) = (self.0.as_bytes(), presented.as_bytes());
        want.len() == got.len() && want.iter().zip(got).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiToken(<set>)")
    }
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |k: &str| lookup(k).filter(|v| !v.trim().is_empty());
        let demo = match get("PEEPHOLE_DEMO").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => bail!("PEEPHOLE_DEMO must be 0 or 1, got {other:?}"),
        };
        let config = get("PEEPHOLE_CONFIG").map(PathBuf::from);
        if config.is_none() && !demo {
            bail!("PEEPHOLE_CONFIG is not set: point it at the hub's YAML config (or set PEEPHOLE_DEMO=1)");
        }
        let listen_text = get("PEEPHOLE_LISTEN").unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
        let listen =
            listen_text.parse().with_context(|| format!("PEEPHOLE_LISTEN must be host:port, got {listen_text:?}"))?;
        let firmware_dir = get("PEEPHOLE_FIRMWARE_DIR").map(PathBuf::from);
        if let Some(dir) = firmware_dir.as_ref().filter(|d| !d.is_dir()) {
            bail!("PEEPHOLE_FIRMWARE_DIR {} is not a directory", dir.display());
        }
        Ok(Self {
            config,
            listen,
            demo,
            state_file: get("PEEPHOLE_STATE").map(PathBuf::from),
            firmware_dir,
            public_url: get("PEEPHOLE_PUBLIC_URL").map(|u| u.trim_end_matches('/').to_owned()),
            api_token: get("PEEPHOLE_API_TOKEN").map(|t| ApiToken::new(t.trim())),
            set_variables: VARIABLES.iter().copied().filter(|v| get(v).is_some()).collect(),
        })
    }

    /// Each variable the hub reads and whether it was set; never the values.
    pub fn env_status(&self) -> Vec<(&'static str, bool)> {
        VARIABLES.iter().map(|v| (*v, self.set_variables.contains(v))).collect()
    }

    /// Read the config file (or the built-in demo config in demo mode when the file is absent).
    pub fn load_config(&self, lookup: impl Fn(&str) -> Option<String>) -> Result<Config> {
        let cfg = match &self.config {
            Some(path) if !self.demo || path.exists() => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading PEEPHOLE_CONFIG {}", path.display()))?;
                Config::parse(&text, lookup).with_context(|| format!("parsing {}", path.display()))?
            }
            _ => Config::parse(DEMO_CONFIG, lookup).context("parsing the built-in demo config")?,
        };
        if self.demo {
            check_demo_kinds(&cfg)?;
        }
        Ok(cfg)
    }
}

fn check_demo_kinds(cfg: &Config) -> Result<()> {
    let rejected: Vec<String> = cfg
        .plugins
        .iter()
        .filter(|p| !DEMO_KINDS.contains(&p.kind.as_str()))
        .map(|p| format!("{} (kind {})", p.name(), p.kind))
        .collect();
    if !rejected.is_empty() {
        bail!("PEEPHOLE_DEMO=1 allows only {DEMO_KINDS:?} plugins; rejected: {}", rejected.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    fn write_config(dir: &tempfile::TempDir, yaml: &str) -> String {
        let path = dir.path().join("peephole.yaml");
        std::fs::write(&path, yaml).unwrap();
        path.display().to_string()
    }

    #[test]
    fn config_path_is_required_outside_demo_mode() {
        let err = Settings::from_lookup(env(&[])).unwrap_err();
        assert!(err.to_string().contains("PEEPHOLE_CONFIG"), "{err}");
    }

    #[test]
    fn defaults_apply_when_only_the_config_is_set() {
        let s = Settings::from_lookup(env(&[("PEEPHOLE_CONFIG", "/etc/peephole.yaml")])).unwrap();
        assert_eq!(s.config, Some(PathBuf::from("/etc/peephole.yaml")));
        assert_eq!(s.listen, "0.0.0.0:8080".parse().unwrap());
        assert!(!s.demo);
        assert_eq!(s.state_file, None);
        assert_eq!(s.firmware_dir, None);
        assert_eq!(s.public_url, None);
    }

    #[test]
    fn reads_every_variable() {
        let dir = tempfile::tempdir().unwrap();
        let fw = dir.path().display().to_string();
        let s = Settings::from_lookup(env(&[
            ("PEEPHOLE_CONFIG", "/c.yaml"),
            ("PEEPHOLE_LISTEN", "127.0.0.1:9000"),
            ("PEEPHOLE_STATE", "/var/lib/peephole/devices.json"),
            ("PEEPHOLE_FIRMWARE_DIR", &fw),
            ("PEEPHOLE_PUBLIC_URL", "https://peephole.example.invalid"),
        ]))
        .unwrap();
        assert_eq!(s.listen, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(s.state_file, Some(PathBuf::from("/var/lib/peephole/devices.json")));
        assert_eq!(s.firmware_dir, Some(PathBuf::from(&fw)));
        assert_eq!(s.public_url.as_deref(), Some("https://peephole.example.invalid"));
    }

    #[test]
    fn api_token_is_trimmed_and_never_printed() {
        let s =
            Settings::from_lookup(env(&[("PEEPHOLE_CONFIG", "/c"), ("PEEPHOLE_API_TOKEN", "  s3cret-tok  ")])).unwrap();
        let token = s.api_token.as_ref().expect("token set");
        assert!(token.matches("s3cret-tok"));
        assert!(!token.matches("s3cret-to"));
        assert!(!token.matches("s3cret-tok "));
        assert!(!format!("{s:?}").contains("s3cret"), "{s:?}");
    }

    #[test]
    fn an_empty_api_token_is_unset() {
        let s = Settings::from_lookup(env(&[("PEEPHOLE_CONFIG", "/c"), ("PEEPHOLE_API_TOKEN", "   ")])).unwrap();
        assert!(s.api_token.is_none());
        assert!(Settings::from_lookup(env(&[("PEEPHOLE_CONFIG", "/c")])).unwrap().api_token.is_none());
    }

    #[test]
    fn env_status_lists_every_known_variable_and_whether_it_was_set() {
        let s = Settings::from_lookup(env(&[
            ("PEEPHOLE_CONFIG", "/c"),
            ("PEEPHOLE_API_TOKEN", "t"),
            ("PEEPHOLE_STATE", " "),
            ("OTHER", "x"),
        ]))
        .unwrap();
        assert_eq!(
            s.env_status(),
            vec![
                ("PEEPHOLE_CONFIG", true),
                ("PEEPHOLE_LISTEN", false),
                ("PEEPHOLE_DEMO", false),
                ("PEEPHOLE_STATE", false),
                ("PEEPHOLE_FIRMWARE_DIR", false),
                ("PEEPHOLE_PUBLIC_URL", false),
                ("PEEPHOLE_API_TOKEN", true),
            ]
        );
    }

    #[test]
    fn invalid_listen_address_names_the_variable() {
        let err = Settings::from_lookup(env(&[("PEEPHOLE_CONFIG", "/c"), ("PEEPHOLE_LISTEN", "nope")])).unwrap_err();
        assert!(err.to_string().contains("PEEPHOLE_LISTEN"), "{err}");
    }

    #[test]
    fn demo_flag_accepts_only_zero_or_one() {
        assert!(Settings::from_lookup(env(&[("PEEPHOLE_DEMO", "1")])).unwrap().demo);
        assert!(!Settings::from_lookup(env(&[("PEEPHOLE_CONFIG", "/c"), ("PEEPHOLE_DEMO", "0")])).unwrap().demo);
        let err = Settings::from_lookup(env(&[("PEEPHOLE_CONFIG", "/c"), ("PEEPHOLE_DEMO", "yes")])).unwrap_err();
        assert!(err.to_string().contains("PEEPHOLE_DEMO"), "{err}");
    }

    #[test]
    fn firmware_dir_must_exist() {
        let err = Settings::from_lookup(env(&[
            ("PEEPHOLE_CONFIG", "/c"),
            ("PEEPHOLE_FIRMWARE_DIR", "/nonexistent/peephole-firmware"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("PEEPHOLE_FIRMWARE_DIR"), "{err}");
    }

    #[test]
    fn loads_the_config_file_with_env_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "hub: {title: \"${T}\"}\npages: [{name: a}]\n");
        let lookup = env(&[("PEEPHOLE_CONFIG", &path), ("T", "Home")]);
        let s = Settings::from_lookup(&lookup).unwrap();
        let cfg = s.load_config(&lookup).unwrap();
        assert_eq!(cfg.hub.title, "Home");
        assert_eq!(cfg.pages.len(), 1);
    }

    #[test]
    fn missing_config_file_is_an_error_outside_demo_mode() {
        let lookup = env(&[("PEEPHOLE_CONFIG", "/nonexistent/peephole.yaml")]);
        let s = Settings::from_lookup(&lookup).unwrap();
        let err = s.load_config(&lookup).unwrap_err();
        assert!(format!("{err:#}").contains("/nonexistent/peephole.yaml"), "{err:#}");
    }

    #[test]
    fn demo_mode_uses_the_built_in_config_when_the_file_is_absent() {
        let lookup = env(&[("PEEPHOLE_DEMO", "1"), ("PEEPHOLE_CONFIG", "/nonexistent/peephole.yaml")]);
        let cfg = Settings::from_lookup(&lookup).unwrap().load_config(&lookup).unwrap();
        assert!(!cfg.pages.is_empty());
        assert!(cfg.plugins.iter().all(|p| p.kind == "clock" || p.kind == "demo"));
    }

    #[test]
    fn demo_mode_rejects_other_plugin_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "plugins: [{kind: clock}, {kind: prometheus, name: prom}]\n");
        let lookup = env(&[("PEEPHOLE_DEMO", "1"), ("PEEPHOLE_CONFIG", &path)]);
        let err = Settings::from_lookup(&lookup).unwrap().load_config(&lookup).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("prometheus") && msg.contains("prom"), "{msg}");
    }
}
