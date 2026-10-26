use std::collections::HashMap;
use std::path::PathBuf;

use super::*;

const TEST_CA: &str = "-----BEGIN CERTIFICATE-----
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
        ("clusters: [{name: both, url: 'https://x.example.invalid', token: t, ca_pem: x, insecure: true}]", "insecure"),
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
    let err = error("clusters: [{name: lab, url: 'https://x.example.invalid', token: t, ca_pem: 'not a certificate'}]");
    assert!(err.contains("\"lab\"") && err.contains("ca_pem"), "{err}");
    let broken = TEST_CA.replace("MIIBpjCC", "MIIBpjXX");
    let err = format!(
        "{:#}",
        build(&format!("clusters: [{{name: lab, url: 'https://x.example.invalid', token: t, ca_pem: {broken:?}}}]"))
            .err()
            .unwrap()
    );
    assert!(err.contains("\"lab\""), "{err}");
}

#[test]
fn cluster_names_must_be_present_and_distinct() {
    assert!(error("clusters: []").contains("clusters"));
    let remote = |name: &str| format!("{{name: '{name}', url: 'https://x.example.invalid', token: t, insecure: true}}");
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

mod poll {
    use std::collections::BTreeMap;

    use serde_json::{Value as Json, json};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    pub(super) const NOW: &str = "2026-10-25T12:00:00Z";

    pub(super) fn now() -> DateTime<Utc> {
        NOW.parse().unwrap()
    }

    fn list(kind: &str, api_version: &str, items: Json) -> Json {
        json!({"kind": kind, "apiVersion": api_version, "metadata": {"resourceVersion": "12784788"}, "items": items})
    }

    fn meta(name: &str, namespace: Option<&str>) -> Json {
        let mut m = json!({"name": name, "uid": format!("uid-{name}"), "resourceVersion": "1", "creationTimestamp": "2026-10-08T01:23:14Z"});
        if let Some(ns) = namespace {
            m["namespace"] = json!(ns);
        }
        m
    }

    fn ready(status: &str, reason: &str) -> Json {
        json!([{"type": "Ready", "status": status, "reason": reason, "message": "", "lastTransitionTime": "2026-10-25T11:00:00Z"}])
    }

    pub(super) fn version() -> Json {
        json!({"major": "1", "minor": "37", "gitVersion": "v1.37.1", "gitCommit": "f78e722310e50bcaca9276be22276d9e91d91308", "gitTreeState": "clean", "buildDate": "2026-09-23T17:06:23Z", "goVersion": "go1.26.8", "compiler": "gc", "platform": "linux/amd64"})
    }

    pub(super) fn nodes() -> Json {
        let node = |name: &str, ready_status: &str, cpu: &str, memory: &str, kubelet: &str| {
            json!({
                "metadata": meta(name, None),
                "status": {
                    "capacity": {"cpu": "2", "memory": memory, "pods": "110"},
                    "allocatable": {"cpu": cpu, "ephemeral-storage": "28514975697", "hugepages-2Mi": "0", "memory": memory, "pods": "110"},
                    "conditions": [
                        {"type": "MemoryPressure", "status": "False", "reason": "KubeletHasSufficientMemory"},
                        {"type": "Ready", "status": ready_status, "reason": "KubeletReady"}
                    ],
                    "nodeInfo": {"kubeletVersion": kubelet, "containerRuntimeVersion": "containerd://2.1.4", "osImage": "Talos (v1.14.2)"}
                }
            })
        };
        list(
            "NodeList",
            "v1",
            json!([node("cp1", "True", "2", "4Gi", "v1.37.1"), node("w1", "False", "1950m", "8388608Ki", "v1.37.0")]),
        )
    }

    fn node_metrics() -> Json {
        let m = |name: &str, cpu: &str, memory: &str| json!({"metadata": meta(name, None), "timestamp": "2026-10-25T11:59:50Z", "window": "10.059s", "usage": {"cpu": cpu, "memory": memory}});
        list(
            "NodeMetricsList",
            "metrics.k8s.io/v1beta1",
            json!([m("cp1", "500000000n", "1Gi"), m("w1", "975m", "2Gi")]),
        )
    }

    fn pods() -> Json {
        let pod = |ns: &str, name: &str, phase: &str, restarts: &[u32]| {
            let statuses: Vec<Json> = restarts
                .iter()
                .enumerate()
                .map(|(i, r)| json!({"name": format!("c{i}"), "ready": phase == "Running", "restartCount": r, "image": "example.invalid/app:1"}))
                .collect();
            let mut status = json!({"phase": phase, "podIP": "10.244.0.10"});
            if !statuses.is_empty() {
                status["containerStatuses"] = json!(statuses);
            }
            json!({"metadata": meta(name, Some(ns)), "spec": {"nodeName": "w1"}, "status": status})
        };
        list(
            "PodList",
            "v1",
            json!([
                pod("argocd", "argocd-server-6d4f", "Running", &[2]),
                pod("default", "job-x-abcde", "Succeeded", &[0]),
                pod("default", "web-1", "Running", &[1, 3]),
                pod("media", "jellyfin-0", "Pending", &[])
            ]),
        )
    }

    fn pvcs() -> Json {
        list(
            "PersistentVolumeClaimList",
            "v1",
            json!([{"metadata": meta("jellyfin-config", Some("media")), "spec": {"storageClassName": "proxmox-zfs"}, "status": {"phase": "Bound", "capacity": {"storage": "10Gi"}}}]),
        )
    }

    fn events() -> Json {
        let event = |ns: &str, object: &str, reason: &str, last: Json, event_time: Json, series: Json| {
            json!({
                "metadata": meta(&format!("{object}.18e1d386f4f27b14"), Some(ns)),
                "involvedObject": {"kind": "Pod", "namespace": ns, "name": object, "apiVersion": "v1"},
                "reason": reason, "message": "", "type": "Warning", "count": 3,
                "firstTimestamp": last.clone(), "lastTimestamp": last, "eventTime": event_time, "series": series,
                "source": {"component": "default-scheduler"}
            })
        };
        list(
            "EventList",
            "v1",
            json!([
                event("default", "web-1", "BackOff", json!("2026-10-25T09:00:00Z"), Json::Null, Json::Null),
                event(
                    "media",
                    "jellyfin-0",
                    "FailedScheduling",
                    Json::Null,
                    json!("2026-10-25T11:40:00.000000Z"),
                    json!({"count": 4, "lastObservedTime": "2026-10-25T11:55:00.000000Z"})
                )
            ]),
        )
    }

    fn applications() -> Json {
        let app = |name: &str, sync: &str, health: &str| {
            json!({"apiVersion": "argoproj.io/v1alpha1", "kind": "Application", "metadata": meta(name, Some("argocd")),
                   "spec": {"project": "default"},
                   "status": {"sync": {"status": sync, "revision": "72d992b8"}, "health": {"status": health}}})
        };
        list(
            "ApplicationList",
            "argoproj.io/v1alpha1",
            json!([
                app("apps-root", "Synced", "Healthy"),
                app("jellyfin", "OutOfSync", "Degraded"),
                app("web", "Synced", "Progressing")
            ]),
        )
    }

    fn kustomizations() -> Json {
        list(
            "KustomizationList",
            "kustomize.toolkit.fluxcd.io/v1",
            json!([{"metadata": meta("apps", Some("flux-system")), "spec": {"interval": "10m"}, "status": {"conditions": ready("True", "ReconciliationSucceeded")}}]),
        )
    }

    fn helmreleases() -> Json {
        list(
            "HelmReleaseList",
            "helm.toolkit.fluxcd.io/v2",
            json!([{"metadata": meta("jellyfin", Some("media")), "spec": {"interval": "30m"}, "status": {"conditions": ready("False", "InstallFailed")}}]),
        )
    }

    fn certificates() -> Json {
        let cert = |ns: &str, name: &str, status: &str, not_after: &str| {
            json!({"metadata": meta(name, Some(ns)), "spec": {"secretName": name},
                   "status": {"conditions": ready(status, "Ready"), "notAfter": not_after, "notBefore": "2026-07-08T12:00:00Z", "revision": 1}})
        };
        list(
            "CertificateList",
            "cert-manager.io/v1",
            json!([
                cert("default", "web-tls", "True", "2027-01-06T12:00:00Z"),
                cert("media", "old-tls", "False", "2026-10-20T12:00:00Z")
            ]),
        )
    }

    pub(super) async fn mount(server: &MockServer, token: &str, route: &str, body: Json) {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// An API server answering every endpoint the plugin reads.
    pub(super) async fn api_server(token: &str, skip: &[&str]) -> MockServer {
        let server = MockServer::start().await;
        let routes = [
            ("/version", version()),
            ("/api/v1/nodes", nodes()),
            ("/apis/metrics.k8s.io/v1beta1/nodes", node_metrics()),
            ("/api/v1/persistentvolumeclaims", pvcs()),
            ("/apis/argoproj.io/v1alpha1/applications", applications()),
            ("/apis/kustomize.toolkit.fluxcd.io/v1/kustomizations", kustomizations()),
            ("/apis/helm.toolkit.fluxcd.io/v2/helmreleases", helmreleases()),
            ("/apis/cert-manager.io/v1/certificates", certificates()),
        ];
        for (route, body) in routes {
            if !skip.contains(&route) {
                mount(&server, token, route, body).await;
            }
        }
        Mock::given(method("GET"))
            .and(path("/api/v1/pods"))
            .and(query_param("limit", "5000"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(pods()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/events"))
            .and(query_param("fieldSelector", "type=Warning"))
            .and(query_param("limit", "500"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(events()))
            .mount(&server)
            .await;
        server
    }

    pub(super) fn remote(name: &str, server: &MockServer, token: &str) -> String {
        format!("{{name: {name}, url: '{}', token: '{token}', ca_pem: {TEST_CA:?}}}", server.uri())
    }

    pub(super) async fn poll(yaml: &str) -> BTreeMap<String, Value> {
        build(yaml).unwrap().poll_at(now()).await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn reads_every_endpoint_of_a_cluster() {
        let server = api_server(TOKEN, &[]).await;
        let got = poll(&format!("clusters: [{}]", remote("home", &server, TOKEN))).await;
        let n = |k: &str| {
            got.get(k).and_then(Value::as_f64).unwrap_or_else(|| panic!("{k} is not a number: {:?}", got.get(k)))
        };
        let t = |k: &str| got.get(k).and_then(Value::as_text).unwrap_or_else(|| panic!("{k} is not text")).to_owned();

        assert_eq!(t("home/version"), "v1.37.1");

        assert_eq!(n("home/nodes_total"), 2.0);
        assert_eq!(n("home/nodes_ready"), 1.0);
        assert_eq!(n("home/node/cp1/ready"), 1.0);
        assert_eq!(n("home/node/w1/ready"), 0.0);
        assert_eq!(t("home/node/w1/version"), "v1.37.0");
        assert_eq!(n("home/node/w1/cpu_alloc"), 1.95);
        assert_eq!(n("home/node/cp1/mem_alloc_bytes"), 4.0 * 1024f64.powi(3));
        assert_eq!(n("home/node/w1/mem_alloc_bytes"), 8.0 * 1024f64.powi(3));
        assert_eq!(n("home/node/cp1/pods_alloc"), 110.0);

        assert_eq!(n("home/node/cp1/cpu"), 0.25);
        assert_eq!(n("home/node/w1/cpu"), 0.5);
        assert_eq!(n("home/node/cp1/mem"), 0.25);
        assert_eq!(n("home/node/w1/mem"), 0.25);
        assert!((n("home/cpu") - 1.475 / 3.95).abs() < 1e-12);
        assert_eq!(n("home/mem"), 0.25);
        assert!(!got.contains_key("home/node/cp1/cpu_cores"));

        assert_eq!(n("home/pods_total"), 4.0);
        assert_eq!(n("home/pods_running"), 2.0);
        assert_eq!(n("home/pods_succeeded"), 1.0);
        assert_eq!(n("home/pods_bad"), 1.0);
        assert_eq!(n("home/pods_pending"), 1.0);
        assert_eq!(n("home/pods_failed"), 0.0);
        assert_eq!(n("home/restarts"), 6.0);
        assert_eq!(n("home/ns/argocd/pods"), 1.0);
        assert_eq!(n("home/ns/default/pods"), 2.0);
        assert_eq!(n("home/ns/media/pods"), 1.0);
        assert_eq!(n("home/namespaces"), 3.0);
        assert_eq!(t("home/top_ns"), "default");
        assert_eq!(n("home/top_ns_pods"), 2.0);
        assert_eq!(t("home/bad_pod"), "media/jellyfin-0");

        assert_eq!(n("home/pvc_total"), 1.0);
        assert_eq!(n("home/pvc_bound"), 1.0);
        assert_eq!(n("home/pvc_pending"), 0.0);

        assert_eq!(n("home/warnings_1h"), 1.0);
        assert_eq!(t("home/last_warning"), "media/jellyfin-0: FailedScheduling");
        assert_eq!(n("home/last_warning_age"), 300.0);

        assert_eq!(n("home/argo_apps"), 3.0);
        assert_eq!(n("home/argo_synced"), 2.0);
        assert_eq!(n("home/argo_outofsync"), 1.0);
        assert_eq!(n("home/argo_healthy"), 1.0);
        assert_eq!(n("home/argo_degraded"), 1.0);
        assert_eq!(n("home/argo_progressing"), 1.0);
        assert_eq!(n("home/argo_missing"), 0.0);
        assert_eq!(n("home/argo/apps-root/synced"), 1.0);
        assert_eq!(n("home/argo/apps-root/healthy"), 1.0);
        assert_eq!(n("home/argo/jellyfin/synced"), 0.0);
        assert_eq!(n("home/argo/jellyfin/healthy"), 0.0);
        assert_eq!(t("home/argo/web/health"), "Progressing");
        assert_eq!(t("home/argo_worst"), "jellyfin: Degraded");

        assert_eq!(n("home/flux_total"), 2.0);
        assert_eq!(n("home/flux_ready"), 1.0);
        assert_eq!(n("home/flux_notready"), 1.0);
        assert_eq!(t("home/flux_worst"), "helmrelease/media/jellyfin: InstallFailed");
        assert_eq!(n("home/flux/kustomization/apps/ready"), 1.0);
        assert_eq!(n("home/flux/helmrelease/jellyfin/ready"), 0.0);

        assert_eq!(n("home/certs_total"), 2.0);
        assert_eq!(n("home/certs_ready"), 1.0);
        assert_eq!(n("home/certs_notready"), 1.0);
        assert_eq!(n("home/cert_min_days"), -5.0);
        assert_eq!(t("home/cert_soonest"), "media/old-tls");
        assert_eq!(n("home/cert/web-tls/days"), 73.0);
        assert_eq!(n("home/cert/web-tls/ready"), 1.0);
        assert_eq!(n("home/cert/old-tls/ready"), 0.0);

        assert_eq!(n("clusters_total"), 1.0);
        assert_eq!(n("clusters_ok"), 1.0);
        assert_eq!(n("_requests_ok"), 10.0);
        assert_eq!(n("_requests_failed"), 0.0);
    }

    #[tokio::test]
    async fn a_missing_metrics_server_is_a_soft_failure() {
        let server = api_server(TOKEN, &["/apis/metrics.k8s.io/v1beta1/nodes"]).await;
        let got = poll(&format!("clusters: [{}]", remote("home", &server, TOKEN))).await;
        assert_eq!(got["home/nodes_total"], Value::Number(2.0));
        assert_eq!(got["home/argo_apps"], Value::Number(3.0));
        assert!(!got.contains_key("home/cpu") && !got.contains_key("home/node/cp1/cpu"));
        assert_eq!(got["_requests_ok"], Value::Number(9.0));
        assert_eq!(got["_requests_failed"], Value::Number(1.0));
    }

    #[tokio::test]
    async fn missing_argo_flux_and_cert_manager_groups_are_soft_failures() {
        let skip = [
            "/apis/argoproj.io/v1alpha1/applications",
            "/apis/kustomize.toolkit.fluxcd.io/v1/kustomizations",
            "/apis/cert-manager.io/v1/certificates",
        ];
        let server = api_server(TOKEN, &skip).await;
        let got = poll(&format!("clusters: [{}]", remote("home", &server, TOKEN))).await;
        assert_eq!(got["home/pods_total"], Value::Number(4.0));
        assert!(!got.keys().any(|k| k.starts_with("home/argo") || k.starts_with("home/cert")));
        assert_eq!(got["home/flux_total"], Value::Number(1.0));
        assert_eq!(got["home/flux_worst"], Value::Text("helmrelease/media/jellyfin: InstallFailed".into()));
        assert!(!got.contains_key("home/flux/kustomization/apps/ready"));
        assert_eq!(got["_requests_failed"], Value::Number(3.0));
    }

    #[tokio::test]
    async fn reads_several_clusters_and_counts_the_ones_that_answer() {
        let home = api_server(TOKEN, &[]).await;
        let lab = api_server("lab-token-not-real", &[]).await;
        let down = MockServer::start().await;
        let yaml = format!(
            "clusters: [{}, {}, {}]",
            remote("home", &home, TOKEN),
            remote("lab", &lab, "lab-token-not-real"),
            remote("down", &down, "down-token")
        );
        let got = poll(&yaml).await;
        assert_eq!(got["home/nodes_total"], Value::Number(2.0));
        assert_eq!(got["lab/nodes_total"], Value::Number(2.0));
        assert_eq!(got["lab/version"], Value::Text("v1.37.1".into()));
        assert!(!got.keys().any(|k| k.starts_with("down/")));
        assert_eq!(got["clusters_total"], Value::Number(3.0));
        assert_eq!(got["clusters_ok"], Value::Number(2.0));
        assert_eq!(got["_requests_ok"], Value::Number(20.0));
        assert_eq!(got["_requests_failed"], Value::Number(10.0));
    }

    #[tokio::test]
    async fn every_cluster_failing_is_an_error_that_never_shows_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
        let k = build(&format!("clusters: [{}]", remote("home", &server, TOKEN))).unwrap();
        let err = format!("{:#}", k.poll_at(now()).await.unwrap_err());
        assert!(err.contains("home /api/v1/nodes") && err.contains("401"), "{err}");
        assert!(!err.contains(TOKEN), "{err}");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 10);
        assert!(requests.iter().all(|r| r.headers["authorization"] == format!("Bearer {TOKEN}").as_str()));
    }

    #[tokio::test]
    async fn disabled_endpoints_are_not_requested() {
        let server = api_server(TOKEN, &[]).await;
        let yaml = format!(
            "{{argocd: false, flux: false, certificates: false, metrics: false, events: false, pods: false, clusters: [{}]}}",
            remote("home", &server, TOKEN)
        );
        let got = poll(&yaml).await;
        let paths: Vec<String> =
            server.received_requests().await.unwrap().iter().map(|r| r.url.path().to_owned()).collect();
        assert_eq!(paths.len(), 3, "{paths:?}");
        for p in ["/version", "/api/v1/nodes", "/api/v1/persistentvolumeclaims"] {
            assert!(paths.iter().any(|seen| seen == p), "{p} not requested: {paths:?}");
        }
        assert!(got.contains_key("home/pvc_total") && !got.contains_key("home/pods_total"));
        assert_eq!(got["_requests_ok"], Value::Number(3.0));
    }

    fn pod_page(names: &[&str], next: Option<&str>) -> Json {
        let items: Vec<Json> = names
            .iter()
            .map(|n| json!({"metadata": meta(n, Some("default")), "status": {"phase": "Running"}}))
            .collect();
        let mut page = list("PodList", "v1", json!(items));
        if let Some(token) = next {
            page["metadata"]["continue"] = json!(token);
            page["metadata"]["remainingItemCount"] = json!(1);
        }
        page
    }

    #[tokio::test]
    async fn follows_pod_pages() {
        use wiremock::matchers::query_param_is_missing;

        let server = MockServer::start().await;
        let auth = || header("authorization", format!("Bearer {TOKEN}"));
        Mock::given(method("GET"))
            .and(path("/api/v1/pods"))
            .and(query_param("limit", "5000"))
            .and(query_param_is_missing("continue"))
            .and(auth())
            .respond_with(
                ResponseTemplate::new(200).set_body_json(pod_page(&["a", "b"], Some("eyJ2IjoibWV0YS5rOHMuaW8vdjEifQ"))),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/pods"))
            .and(query_param("limit", "5000"))
            .and(query_param("continue", "eyJ2IjoibWV0YS5rOHMuaW8vdjEifQ"))
            .and(auth())
            .respond_with(ResponseTemplate::new(200).set_body_json(pod_page(&["c"], None)))
            .mount(&server)
            .await;
        let got = poll(&format!("clusters: [{}]", remote("home", &server, TOKEN))).await;
        assert_eq!(got["home/pods_total"], Value::Number(3.0));
        assert_eq!(got["home/pods_running"], Value::Number(3.0));
    }

    #[tokio::test]
    async fn more_than_five_pod_pages_fails_the_pods_part_only() {
        let server = api_server(TOKEN, &[]).await;
        server.reset().await;
        mount(&server, TOKEN, "/api/v1/nodes", nodes()).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/pods"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pod_page(&["a"], Some("more"))))
            .expect(5)
            .mount(&server)
            .await;
        let k = build(&format!(
            "{{argocd: false, flux: false, certificates: false, metrics: false, events: false, clusters: [{}]}}",
            remote("home", &server, TOKEN)
        ))
        .unwrap();
        let got: BTreeMap<_, _> = k.poll_at(now()).await.unwrap().into_iter().collect();
        assert!(!got.contains_key("home/pods_total"));
        assert_eq!(got["home/nodes_total"], Value::Number(2.0));
        assert_eq!(got["_requests_failed"], Value::Number(3.0));
    }
}

mod values_edges {
    use chrono::{DateTime, Utc};
    use peephole_core::Value;
    use serde_json::json;

    use super::super::{api, values};

    fn now() -> DateTime<Utc> {
        super::poll::NOW.parse().unwrap()
    }

    fn get(values: &values::Values, key: &str) -> Option<Value> {
        values.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn items<T: serde::de::DeserializeOwned>(raw: serde_json::Value) -> Vec<T> {
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn metrics_without_the_node_list_report_raw_cores() {
        let usage: Vec<api::NodeMetrics> =
            items(json!([{"metadata": {"name": "cp1"}, "usage": {"cpu": "1500m", "memory": "1Gi"}}]));
        let got = values::metrics(&usage, None).unwrap();
        assert_eq!(get(&got, "node/cp1/cpu_cores"), Some(Value::Number(1.5)));
        assert!(
            get(&got, "node/cp1/cpu").is_none() && get(&got, "node/cp1/mem").is_none() && get(&got, "cpu").is_none()
        );
    }

    #[test]
    fn a_bad_quantity_fails_naming_the_node() {
        let usage: Vec<api::NodeMetrics> = items(json!([{"metadata": {"name": "cp1"}, "usage": {"cpu": "lots"}}]));
        let err = values::metrics(&usage, None).unwrap_err().to_string();
        assert!(err.contains("cp1") && err.contains("lots"), "{err}");
    }

    #[test]
    fn nothing_wrong_means_empty_texts() {
        let events = values::events(&[], now());
        assert_eq!(get(&events, "warnings_1h"), Some(Value::Number(0.0)));
        assert_eq!(get(&events, "last_warning"), Some(Value::Text(String::new())));
        assert!(get(&events, "last_warning_age").is_none());

        let apps: Vec<api::Application> = items(
            json!([{"metadata": {"name": "a", "namespace": "argocd"}, "status": {"sync": {"status": "Synced"}, "health": {"status": "Healthy"}}}]),
        );
        assert_eq!(get(&values::argo(&apps), "argo_worst"), Some(Value::Text(String::new())));

        let flux: Vec<api::Reconciled> = items(
            json!([{"metadata": {"name": "apps", "namespace": "flux-system"}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}}]),
        );
        assert_eq!(
            get(&values::flux_totals(&[("kustomization", &flux)]), "flux_worst"),
            Some(Value::Text(String::new()))
        );

        let pods = values::pods(&[]);
        assert_eq!(get(&pods, "bad_pod"), Some(Value::Text(String::new())));
        assert!(get(&pods, "top_ns").is_none());
    }

    #[test]
    fn an_app_that_is_healthy_but_out_of_sync_is_the_worst_one() {
        let apps: Vec<api::Application> = items(json!([
            {"metadata": {"name": "a"}, "status": {"sync": {"status": "Synced"}, "health": {"status": "Healthy"}}},
            {"metadata": {"name": "b"}, "status": {"sync": {"status": "OutOfSync"}, "health": {"status": "Healthy"}}},
            {"metadata": {"name": "c"}, "status": {"sync": {"status": "Synced"}, "health": {"status": "Missing"}}}
        ]));
        let got = values::argo(&apps);
        assert_eq!(get(&got, "argo_worst"), Some(Value::Text("b: OutOfSync".into())));
        assert_eq!(get(&got, "argo_missing"), Some(Value::Number(1.0)));
    }

    #[test]
    fn a_flux_object_without_conditions_is_not_ready_for_an_unknown_reason() {
        let flux: Vec<api::Reconciled> = items(json!([{"metadata": {"name": "new", "namespace": "apps"}}]));
        let got = values::flux_totals(&[("helmrelease", &flux)]);
        assert_eq!(get(&got, "flux_worst"), Some(Value::Text("helmrelease/apps/new: Unknown".into())));
    }

    #[test]
    fn warning_time_uses_the_latest_of_the_three_timestamps() {
        let events: Vec<api::Event> = items(json!([
            {"metadata": {"name": "e1", "namespace": "kube-system"}, "involvedObject": {"kind": "Node", "name": "w1"},
             "reason": "Rebooted", "lastTimestamp": "2026-10-25T10:00:00Z", "eventTime": null,
             "series": {"count": 2, "lastObservedTime": "2026-10-25T11:59:00.500000Z"}},
            {"metadata": {"name": "e2", "namespace": "default"}, "involvedObject": {"kind": "Pod", "name": "p"},
             "reason": "Old", "lastTimestamp": null, "eventTime": null},
            {"metadata": {"name": "e3", "namespace": "default"}, "involvedObject": {"kind": "Pod", "name": "q"},
             "reason": "Edge", "lastTimestamp": "2026-10-25T11:00:00Z"}
        ]));
        let got = values::events(&events, now());
        assert_eq!(get(&got, "warnings_1h"), Some(Value::Number(2.0)));
        assert_eq!(get(&got, "last_warning"), Some(Value::Text("kube-system/w1: Rebooted".into())));
        assert_eq!(get(&got, "last_warning_age"), Some(Value::Number(59.0)));
    }
}
