use std::time::Duration;

use anyhow::Context;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;

mod quantity;

/// Where a pod finds its own ServiceAccount credentials.
const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    /// Per request; default the shared HTTP timeout, capped at the interval.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default = "default_true")]
    pub argocd: bool,
    #[serde(default = "default_true")]
    pub flux: bool,
    #[serde(default = "default_true")]
    pub certificates: bool,
    #[serde(default = "default_true")]
    pub metrics: bool,
    #[serde(default = "default_true")]
    pub events: bool,
    #[serde(default = "default_true")]
    pub pods: bool,
    #[serde(default = "default_clusters")]
    pub clusters: Vec<ClusterSettings>,
}

fn default_interval() -> u64 {
    30
}
fn default_true() -> bool {
    true
}
fn default_clusters() -> Vec<ClusterSettings> {
    vec![ClusterSettings { name: "local".into(), in_cluster: true, ..ClusterSettings::default() }]
}

/// One API server. No `Debug`: the entry holds a bearer token.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClusterSettings {
    /// Key segment for this cluster's values.
    pub name: String,
    /// Use the pod's own ServiceAccount: API host from `KUBERNETES_SERVICE_HOST`
    /// and `KUBERNETES_SERVICE_PORT`, token and CA from the mounted secret.
    #[serde(default)]
    pub in_cluster: bool,
    #[serde(default)]
    pub url: Option<String>,
    /// Bearer token; never logged.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_file: Option<String>,
    /// PEM bundle of the cluster CA; the only roots trusted for this cluster.
    #[serde(default)]
    pub ca_pem: Option<String>,
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Accept any server certificate.
    #[serde(default)]
    pub insecure: bool,
}

struct Cluster {
    key: String,
    base: reqwest::Url,
    client: reqwest::Client,
}

pub struct Kubernetes {
    name: String,
    interval: Duration,
    clusters: Vec<Cluster>,
}

impl Kubernetes {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        Self::with_env(name, settings, &|var| std::env::var(var).ok())
    }

    fn with_env(name: &str, settings: Settings, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(s) => crate::period::seconds(s, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        anyhow::ensure!(!settings.clusters.is_empty(), "clusters must list at least one cluster");
        let mut clusters: Vec<Cluster> = Vec::new();
        for entry in &settings.clusters {
            let cluster = Cluster::new(entry, timeout, env).with_context(|| format!("cluster {:?}", entry.name))?;
            anyhow::ensure!(
                clusters.iter().all(|c| c.key != cluster.key),
                "cluster {:?}: duplicate cluster key {:?}",
                entry.name,
                cluster.key
            );
            clusters.push(cluster);
        }
        Ok(Self { name: name.to_owned(), interval, clusters })
    }
}

enum Trust {
    Roots(Vec<reqwest::Certificate>),
    Anything,
}

impl Cluster {
    fn new(entry: &ClusterSettings, timeout: Duration, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        anyhow::ensure!(!entry.name.trim().is_empty(), "name must not be empty");
        let base = crate::http::base_url(&api_url(entry, env)?)?;
        let token = read_token(entry)?;
        let trust = trust(entry)?;
        Ok(Self { key: crate::key::sanitize(&entry.name), base, client: client(timeout, &token, trust)? })
    }
}

fn api_url(entry: &ClusterSettings, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    if let Some(url) = &entry.url {
        return Ok(url.clone());
    }
    anyhow::ensure!(entry.in_cluster, "url is required unless in_cluster is true");
    let host = env("KUBERNETES_SERVICE_HOST").context("in_cluster needs KUBERNETES_SERVICE_HOST in the environment")?;
    let port = env("KUBERNETES_SERVICE_PORT").context("in_cluster needs KUBERNETES_SERVICE_PORT in the environment")?;
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    Ok(format!("https://{host}:{port}"))
}

fn read_token(entry: &ClusterSettings) -> anyhow::Result<String> {
    anyhow::ensure!(
        !(entry.in_cluster && entry.token.is_some()),
        "in_cluster reads its token from token_file, not token"
    );
    let token = match (&entry.token, &entry.token_file) {
        (Some(_), Some(_)) => anyhow::bail!("set token or token_file, not both"),
        (Some(token), None) => token.clone(),
        (None, file) => {
            let path = match file {
                Some(path) => path.clone(),
                None if entry.in_cluster => format!("{SA_DIR}/token"),
                None => anyhow::bail!("token or token_file is required"),
            };
            std::fs::read_to_string(&path).with_context(|| format!("reading token_file {path}"))?
        }
    };
    let token = token.trim().to_owned();
    anyhow::ensure!(!token.is_empty(), "the token is empty");
    Ok(token)
}

fn trust(entry: &ClusterSettings) -> anyhow::Result<Trust> {
    anyhow::ensure!(!(entry.in_cluster && entry.ca_pem.is_some()), "in_cluster reads its CA from ca_file, not ca_pem");
    let pem = match (&entry.ca_pem, &entry.ca_file, entry.insecure) {
        (None, None, true) => return Ok(Trust::Anything),
        (Some(_), _, true) | (_, Some(_), true) => anyhow::bail!("insecure cannot be combined with ca_pem or ca_file"),
        (Some(_), Some(_), false) => anyhow::bail!("set ca_pem or ca_file, not both"),
        (Some(pem), None, false) => pem.clone(),
        (None, file, false) => {
            let path = match file {
                Some(path) => path.clone(),
                None if entry.in_cluster => format!("{SA_DIR}/ca.crt"),
                None => anyhow::bail!("ca_pem, ca_file or insecure: true is required"),
            };
            std::fs::read_to_string(&path).with_context(|| format!("reading ca_file {path}"))?
        }
    };
    let roots = reqwest::Certificate::from_pem_bundle(pem.as_bytes()).context("ca_pem is not a PEM bundle")?;
    anyhow::ensure!(!roots.is_empty(), "ca_pem holds no PEM certificate");
    Ok(Trust::Roots(roots))
}

/// The bearer header is marked sensitive and never formatted into an error.
fn client(timeout: Duration, token: &str, trust: Trust) -> anyhow::Result<reqwest::Client> {
    let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| anyhow::anyhow!("the token is not a valid HTTP header value"))?;
    auth.set_sensitive(true);
    let headers = HeaderMap::from_iter([(AUTHORIZATION, auth)]);
    let builder = reqwest::Client::builder().timeout(timeout).default_headers(headers);
    let builder = match trust {
        Trust::Roots(roots) => builder.tls_certs_only(roots),
        Trust::Anything => builder.tls_danger_accept_invalid_certs(true),
    };
    builder.build().context("building the HTTP client (is ca_pem a valid certificate?)")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;

    pub(super) const TEST_CA: &str = "-----BEGIN CERTIFICATE-----
MIIBpjCCAU2gAwIBAgIUIFnQIOBL42pdzcveTv7q99yJ67kwCgYIKoZIzj0EAwIw
KDEmMCQGA1UEAwwdcGVlcGhvbGUtdGVzdC5leGFtcGxlLmludmFsaWQwIBcNMjYx
MDI2MDEyOTU1WhgPMjEyNjEwMDIwMTI5NTVaMCgxJjAkBgNVBAMMHXBlZXBob2xl
LXRlc3QuZXhhbXBsZS5pbnZhbGlkMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE
+hJzuX4DV+tdSOG+WrhGachAn8ft6V2iTsT5KcQdDnrlQUXho8d0xYpPl3f+WSym
4pozZUxjNapVBoA8kHhG16NTMFEwHQYDVR0OBBYEFPjBoAJQt88I8jSlpLNl8CzQ
BYfNMB8GA1UdIwQYMBaAFPjBoAJQt88I8jSlpLNl8CzQBYfNMA8GA1UdEwEB/wQF
MAMBAf8wCgYIKoZIzj0EAwIDRwAwRAIgAapPelxYs37BuPi3puIkucf9gqa6NRj+
tfezldeT0FQCIEMY5Pvrzt8Eyy1ZRVR+13i/XPEkmVLpfn0m7bClIkpW
-----END CERTIFICATE-----
";

    const TOKEN: &str = "sa-token-not-real-7f3a";

    fn settings(yaml: &str) -> Settings {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn build(yaml: &str) -> anyhow::Result<Kubernetes> {
        Kubernetes::with_env("k8s", settings(yaml), &|_| None)
    }

    fn error(yaml: &str) -> String {
        format!("{:#}", build(yaml).err().expect("settings should be rejected"))
    }

    /// A fresh directory holding a ServiceAccount token and CA like a pod's mount.
    fn sa_dir(test: &str, token: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("peephole-k8s-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), token).unwrap();
        std::fs::write(dir.join("ca.crt"), TEST_CA).unwrap();
        dir
    }

    fn in_cluster_yaml(dir: &std::path::Path) -> String {
        format!(
            "clusters: [{{name: local, in_cluster: true, token_file: '{}', ca_file: '{}'}}]",
            dir.join("token").display(),
            dir.join("ca.crt").display()
        )
    }

    #[test]
    fn defaults() {
        let s = settings("{}");
        assert_eq!(s.interval_s, 30);
        assert!(s.timeout_s.is_none());
        assert!(s.argocd && s.flux && s.certificates && s.metrics && s.events && s.pods);
        assert_eq!(s.clusters.len(), 1);
        assert_eq!(s.clusters[0].name, "local");
        assert!(s.clusters[0].in_cluster && s.clusters[0].url.is_none() && !s.clusters[0].insecure);
    }

    #[test]
    fn in_cluster_reads_the_service_env_and_the_mounted_secret() {
        let dir = sa_dir("env", &format!("{TOKEN}\n"));
        let env = HashMap::from([("KUBERNETES_SERVICE_HOST", "10.43.0.1"), ("KUBERNETES_SERVICE_PORT", "443")]);
        let lookup = |var: &str| env.get(var).map(|v| (*v).to_owned());
        let k = Kubernetes::with_env("k8s", settings(&in_cluster_yaml(&dir)), &lookup).unwrap();
        assert_eq!(k.interval, Duration::from_secs(30));
        assert_eq!(k.clusters.len(), 1);
        assert_eq!(k.clusters[0].key, "local");
        assert_eq!(k.clusters[0].base.as_str(), "https://10.43.0.1/");
    }

    #[test]
    fn in_cluster_brackets_an_ipv6_service_host() {
        let dir = sa_dir("ipv6", TOKEN);
        let env = HashMap::from([("KUBERNETES_SERVICE_HOST", "fd00:10:96::1"), ("KUBERNETES_SERVICE_PORT", "6443")]);
        let lookup = |var: &str| env.get(var).map(|v| (*v).to_owned());
        let k = Kubernetes::with_env("k8s", settings(&in_cluster_yaml(&dir)), &lookup).unwrap();
        assert_eq!(k.clusters[0].base.as_str(), "https://[fd00:10:96::1]:6443/");
    }

    #[test]
    fn in_cluster_url_overrides_the_env() {
        let dir = sa_dir("url", TOKEN);
        let yaml = in_cluster_yaml(&dir).replace("in_cluster: true", "in_cluster: true, url: 'http://127.0.0.1:1'");
        assert_eq!(build(&yaml).unwrap().clusters[0].base.as_str(), "http://127.0.0.1:1/");
    }

    #[test]
    fn in_cluster_without_the_env_names_the_cluster_and_the_variable() {
        let dir = sa_dir("noenv", TOKEN);
        let err = error(&in_cluster_yaml(&dir));
        assert!(err.contains("\"local\"") && err.contains("KUBERNETES_SERVICE_HOST"), "{err}");
    }

    #[test]
    fn a_missing_token_file_fails_fast_naming_the_cluster() {
        let err = error(
            "clusters: [{name: home, in_cluster: true, url: 'https://k.example.invalid', token_file: /nonexistent/peephole/token, insecure: true}]",
        );
        assert!(err.contains("\"home\"") && err.contains("token_file"), "{err}");
    }

    #[test]
    fn remote_clusters_need_a_url_a_token_and_a_ca_or_insecure() {
        let ok = format!(
            "clusters: [{{name: a, url: 'https://a.example.invalid:6443', token: t, ca_pem: {:?}}}, {{name: b, url: 'https://b.example.invalid:6443', token: t, insecure: true}}]",
            TEST_CA
        );
        let k = build(&ok).unwrap();
        assert_eq!(k.clusters.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        for (yaml, needle) in [
            ("clusters: [{name: nourl, token: t, insecure: true}]", "url"),
            ("clusters: [{name: notoken, url: 'https://x.example.invalid', insecure: true}]", "token"),
            ("clusters: [{name: noca, url: 'https://x.example.invalid', token: t}]", "ca_pem"),
            (
                "clusters: [{name: both, url: 'https://x.example.invalid', token: t, ca_pem: x, insecure: true}]",
                "insecure",
            ),
            (
                "clusters: [{name: twotokens, url: 'https://x.example.invalid', token: t, token_file: /f, insecure: true}]",
                "token_file",
            ),
            ("clusters: [{name: badurl, url: 'ftp://x.example.invalid', token: t, insecure: true}]", "url"),
            (
                "clusters: [{name: incl, in_cluster: true, token: t, url: 'https://x.example.invalid', insecure: true}]",
                "in_cluster",
            ),
        ] {
            let err = error(yaml);
            let cluster = yaml.split("name: ").nth(1).unwrap().split(',').next().unwrap();
            assert!(err.contains(&format!("{cluster:?}")) && err.contains(needle), "{yaml}: {err}");
        }
    }

    #[test]
    fn a_ca_that_is_not_pem_is_rejected_naming_the_cluster() {
        let err =
            error("clusters: [{name: lab, url: 'https://x.example.invalid', token: t, ca_pem: 'not a certificate'}]");
        assert!(err.contains("\"lab\"") && err.contains("ca_pem"), "{err}");
        let broken = TEST_CA.replace("MIIBpjCC", "MIIBpjXX");
        let err = format!(
            "{:#}",
            build(&format!(
                "clusters: [{{name: lab, url: 'https://x.example.invalid', token: t, ca_pem: {broken:?}}}]"
            ))
            .err()
            .unwrap()
        );
        assert!(err.contains("\"lab\""), "{err}");
    }

    #[test]
    fn cluster_names_must_be_present_and_distinct() {
        assert!(error("clusters: []").contains("clusters"));
        let remote =
            |name: &str| format!("{{name: '{name}', url: 'https://x.example.invalid', token: t, insecure: true}}");
        assert!(error(&format!("clusters: [{}]", remote(""))).contains("name"));
        let err = error(&format!("clusters: [{}, {}]", remote("a b"), remote("a-b")));
        assert!(err.contains("a-b") && err.contains("duplicate"), "{err}");
    }

    #[test]
    fn a_token_that_is_not_a_header_value_is_rejected_without_echoing_it() {
        let err = error(&format!(
            "clusters: [{{name: x, url: 'https://x.example.invalid', token: \"{TOKEN}\\u0001bad\", insecure: true}}]"
        ));
        assert!(err.contains("\"x\"") && err.contains("token") && !err.contains(TOKEN), "{err}");
    }

    #[test]
    fn interval_and_timeout_must_be_positive() {
        let remote = "clusters: [{name: x, url: 'https://x.example.invalid', token: t, insecure: true}]";
        assert!(error(&format!("{{interval_s: 0, {remote}}}")).contains("interval_s"));
        assert!(error(&format!("{{timeout_s: 0, {remote}}}")).contains("timeout_s"));
        assert_eq!(build(&format!("{{interval_s: 5, {remote}}}")).unwrap().interval, Duration::from_secs(5));
    }
}
