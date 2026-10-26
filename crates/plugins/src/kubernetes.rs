//! Kubernetes clusters read straight from their API servers with a read-only
//! ServiceAccount (`docs/kubernetes-rbac.yaml`): what kube-state-metrics does
//! not give a small screen, such as Argo CD sync and health, Flux readiness,
//! cert-manager expiry, live metrics-server usage and recent warnings.
//!
//! Every poll makes these `GET`s per cluster, each an independent part:
//! - `/version`
//! - `/api/v1/nodes`
//! - `/apis/metrics.k8s.io/v1beta1/nodes` (`metrics`)
//! - `/api/v1/pods?limit=5000`, following `metadata.continue` for up to 5 pages (`pods`)
//!
//! Clusters are polled one after another (each cluster's endpoints together) so the peak memory of a poll is one
//! cluster's responses, not every cluster's at once.
//! - `/api/v1/persistentvolumeclaims`
//! - `/api/v1/events?fieldSelector=type%3DWarning&limit=500` (`events`)
//! - `/apis/argoproj.io/v1alpha1/applications` (`argocd`)
//! - `/apis/kustomize.toolkit.fluxcd.io/v1/kustomizations` and
//!   `/apis/helm.toolkit.fluxcd.io/v2/helmreleases` (`flux`)
//! - `/apis/cert-manager.io/v1/certificates` (`certificates`)
//!
//! A group that is not installed answers 404 (or 403 without RBAC): that part
//! fails and is counted, the rest of the poll goes on.
//!
//! References:
//! - API objects: <https://kubernetes.io/docs/reference/kubernetes-api/>
//! - In-cluster access: <https://kubernetes.io/docs/tasks/run-application/access-api-from-pod/>
//! - Argo CD Application: <https://argo-cd.readthedocs.io/en/stable/operator-manual/application.yaml>
//!   and its health states: <https://argo-cd.readthedocs.io/en/stable/operator-manual/health/>
//! - Flux `Ready` conditions: <https://fluxcd.io/flux/components/kustomize/kustomizations/>,
//!   <https://fluxcd.io/flux/components/helm/helmreleases/>
//! - cert-manager `status.notAfter` and `Ready`: <https://cert-manager.io/docs/reference/api-docs/>

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use peephole_core::{Plugin, Value};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;

mod api;
mod quantity;
mod values;

/// Where a pod finds its own ServiceAccount credentials.
const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

const VERSION: &str = "/version";
const NODES: &str = "/api/v1/nodes";
const NODE_METRICS: &str = "/apis/metrics.k8s.io/v1beta1/nodes";
const PODS: &str = "/api/v1/pods";
const PVCS: &str = "/api/v1/persistentvolumeclaims";
const EVENTS: &str = "/api/v1/events";
const APPLICATIONS: &str = "/apis/argoproj.io/v1alpha1/applications";
const KUSTOMIZATIONS: &str = "/apis/kustomize.toolkit.fluxcd.io/v1/kustomizations";
const HELMRELEASES: &str = "/apis/helm.toolkit.fluxcd.io/v2/helmreleases";
const CERTIFICATES: &str = "/apis/cert-manager.io/v1/certificates";

const POD_PAGE: usize = 5000;
const POD_PAGES: usize = 5;

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
    argocd: bool,
    flux: bool,
    certificates: bool,
    metrics: bool,
    events: bool,
    pods: bool,
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
        Ok(Self {
            name: name.to_owned(),
            interval,
            clusters,
            argocd: settings.argocd,
            flux: settings.flux,
            certificates: settings.certificates,
            metrics: settings.metrics,
            events: settings.events,
            pods: settings.pods,
        })
    }
}

enum Trust {
    /// The CA bundle and the setting it came from (`ca_pem` or `ca_file`).
    Roots(Vec<reqwest::Certificate>, &'static str),
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
    let (pem, field) = match (&entry.ca_pem, &entry.ca_file, entry.insecure) {
        (None, None, true) => return Ok(Trust::Anything),
        (Some(_), _, true) | (_, Some(_), true) => anyhow::bail!("insecure cannot be combined with ca_pem or ca_file"),
        (Some(_), Some(_), false) => anyhow::bail!("set ca_pem or ca_file, not both"),
        (Some(pem), None, false) => (pem.clone(), "ca_pem"),
        (None, file, false) => {
            let path = match file {
                Some(path) => path.clone(),
                None if entry.in_cluster => format!("{SA_DIR}/ca.crt"),
                None => anyhow::bail!("ca_pem, ca_file or insecure: true is required"),
            };
            (std::fs::read_to_string(&path).with_context(|| format!("reading ca_file {path}"))?, "ca_file")
        }
    };
    let roots = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
        .with_context(|| format!("{field} is not a PEM bundle"))?;
    anyhow::ensure!(!roots.is_empty(), "{field} holds no PEM certificate");
    Ok(Trust::Roots(roots, field))
}

/// The bearer header is marked sensitive and never formatted into an error.
fn client(timeout: Duration, token: &str, trust: Trust) -> anyhow::Result<reqwest::Client> {
    let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| anyhow::anyhow!("the token is not a valid HTTP header value"))?;
    auth.set_sensitive(true);
    let headers = HeaderMap::from_iter([(AUTHORIZATION, auth)]);
    let builder = reqwest::Client::builder().timeout(timeout).default_headers(headers);
    match trust {
        Trust::Roots(roots, field) => builder
            .tls_certs_only(roots)
            .build()
            .with_context(|| format!("building the HTTP client (is {field} a valid certificate?)")),
        Trust::Anything => builder.tls_danger_accept_invalid_certs(true).build().context("building the HTTP client"),
    }
}

#[async_trait]
impl Plugin for Kubernetes {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        self.poll_at(Utc::now()).await
    }
}

impl Kubernetes {
    async fn poll_at(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<(String, Value)>> {
        let mut polls = Vec::with_capacity(self.clusters.len());
        for c in &self.clusters {
            polls.push(self.poll_cluster(c, now).await);
        }
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        let mut answered = 0;
        for (cluster, poll) in self.clusters.iter().zip(polls) {
            answered += usize::from(poll.nodes_ok);
            for (path, result) in poll.parts {
                match result {
                    Ok(values) => partial.ok(values.into_iter().map(|(k, v)| (format!("{}/{k}", cluster.key), v))),
                    Err(e) => partial.fail(&format!("{} {path}", cluster.key), &e),
                }
            }
        }
        let mut values = partial.finish()?;
        values.push(("clusters_total".into(), Value::Number(self.clusters.len() as f64)));
        values.push(("clusters_ok".into(), Value::Number(answered as f64)));
        Ok(values)
    }

    async fn poll_cluster(&self, c: &Cluster, now: DateTime<Utc>) -> ClusterPoll {
        let (version, nodes, metrics, pods, pvcs, events, apps, kustomizations, helmreleases, certs) = tokio::join!(
            c.get::<api::Version>(VERSION, &[]),
            c.list::<api::Node>(NODES),
            when(self.metrics, c.list::<api::NodeMetrics>(NODE_METRICS)),
            when(self.pods, c.pods()),
            c.list::<api::Pvc>(PVCS),
            when(
                self.events,
                c.list_query::<api::Event>(EVENTS, &[("fieldSelector", "type=Warning"), ("limit", "500")])
            ),
            when(self.argocd, c.list::<api::Application>(APPLICATIONS)),
            when(self.flux, c.list::<api::Reconciled>(KUSTOMIZATIONS)),
            when(self.flux, c.list::<api::Reconciled>(HELMRELEASES)),
            when(self.certificates, c.list::<api::Certificate>(CERTIFICATES)),
        );
        let mut parts: Vec<(&'static str, anyhow::Result<values::Values>)> = Vec::new();
        parts.push((VERSION, version.map(values::version)));
        if let Some(metrics) = metrics {
            let listed = nodes.as_ref().ok().map(Vec::as_slice);
            parts.push((NODE_METRICS, metrics.and_then(|m| values::metrics(&m, listed))));
        }
        let nodes_ok = nodes.is_ok();
        parts.push((NODES, nodes.and_then(|n| values::nodes(&n))));
        if let Some(pods) = pods {
            parts.push((PODS, pods.map(|p| values::pods(&p))));
        }
        parts.push((PVCS, pvcs.map(|p| values::pvcs(&p))));
        if let Some(events) = events {
            parts.push((EVENTS, events.map(|e| values::events(&e, now))));
        }
        if let Some(apps) = apps {
            parts.push((APPLICATIONS, apps.map(|a| values::argo(&a))));
        }
        let flux: Vec<_> =
            [("kustomization", KUSTOMIZATIONS, kustomizations), ("helmrelease", HELMRELEASES, helmreleases)]
                .into_iter()
                .filter_map(|(kind, path, result)| result.map(|r| (kind, path, r)))
                .collect();
        let answered: Vec<(&str, &[api::Reconciled])> =
            flux.iter().filter_map(|(kind, _, r)| r.as_ref().ok().map(|o| (*kind, o.as_slice()))).collect();
        let mut totals = (!answered.is_empty()).then(|| values::flux_totals(&answered));
        for (kind, path, result) in flux {
            parts.push((
                path,
                result.map(|objects| {
                    let mut out = values::flux_objects(kind, &objects);
                    out.extend(totals.take().unwrap_or_default());
                    out
                }),
            ));
        }
        if let Some(certs) = certs {
            parts.push((CERTIFICATES, certs.map(|c| values::certificates(&c, now))));
        }
        ClusterPoll { parts, nodes_ok }
    }
}

struct ClusterPoll {
    parts: Vec<(&'static str, anyhow::Result<values::Values>)>,
    nodes_ok: bool,
}

async fn when<T>(enabled: bool, fetch: impl Future<Output = T>) -> Option<T> {
    if enabled { Some(fetch.await) } else { None }
}

impl Cluster {
    fn url(&self, path: &str, query: &[(&str, &str)]) -> reqwest::Url {
        let mut url = crate::http::join(&self.base, path);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<T> {
        let response = crate::http::get(&self.client, self.url(path, query)).await?;
        response.json().await.map_err(|e| anyhow::anyhow!("GET {path}: unexpected body: {}", e.without_url()))
    }

    async fn list<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<Vec<T>> {
        self.list_query(path, &[]).await
    }

    async fn list_query<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<Vec<T>> {
        Ok(self.get::<api::List<T>>(path, query).await?.items)
    }

    /// Every pod, `POD_PAGE` at a time; more than `POD_PAGES` pages is an error.
    async fn pods(&self) -> anyhow::Result<Vec<api::Pod>> {
        let limit = POD_PAGE.to_string();
        let mut pods = Vec::new();
        let mut next: Option<String> = None;
        for _ in 0..POD_PAGES {
            let mut query = vec![("limit", limit.as_str())];
            if let Some(token) = &next {
                query.push(("continue", token.as_str()));
            }
            let page: api::List<api::Pod> = self.get(PODS, &query).await?;
            pods.extend(page.items);
            next = page.metadata.next.filter(|t| !t.is_empty());
            if next.is_none() {
                return Ok(pods);
            }
        }
        anyhow::bail!("GET {PODS}: more than {POD_PAGES} pages of {POD_PAGE} pods")
    }
}

#[cfg(test)]
mod tests;
