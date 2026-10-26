//! Proxmox VE hosts, guests and storage, over the REST API with an API token
//! (`Authorization: PVEAPIToken=<user>@<realm>!<tokenid>=<secret>`, per
//! <https://pve.proxmox.com/wiki/Proxmox_VE_API>). Endpoints, each wrapped in `{"data": …}`:
//! - `GET /api2/json/nodes`: `[{node, status: online|offline|unknown, …}]`
//! - `GET /api2/json/nodes/{node}/status`: `cpu` (fraction), `memory`/`rootfs` `{total, used, …}`
//!   bytes, `uptime` seconds, `pveversion`, `loadavg` (three strings), `kversion`, `swap`
//! - `GET /api2/json/nodes/{node}/qemu` and `/lxc`: `[{vmid, name?, status: running|stopped,
//!   cpu?, mem?, maxmem?, uptime?}]`
//! - `GET /api2/json/nodes/{node}/storage`: `[{storage, type, active?, total?, used?, avail?}]`
//!
//! Checked against the return schemas in <https://pve.proxmox.com/pve-docs/api-viewer/apidoc.js>.
//! `kversion`, `swap` and the string form of `loadavg` are not in that schema (the status return
//! allows additional properties); they were read from the `status` method of pve-manager's
//! `PVE/API2/Nodes.pm`. Perl's JSON writes booleans such as `active` as `1`/`0`, so both forms
//! are accepted.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the hosts hold API token secrets.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Samples kept per node for `cpu_history`/`mem_history`; 0 turns them off.
    #[serde(default = "default_history")]
    pub history: usize,
    #[serde(default = "default_true")]
    pub guests: bool,
    #[serde(default = "default_true")]
    pub storage: bool,
    pub hosts: Vec<HostSettings>,
}

fn default_interval() -> u64 {
    30
}
fn default_history() -> usize {
    60
}
fn default_true() -> bool {
    true
}

/// No `Debug`: holds the token secret.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSettings {
    pub name: String,
    pub url: String,
    pub token_id: String,
    pub token_secret: String,
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

#[derive(Deserialize)]
struct NodeEntry {
    node: String,
    status: String,
}

#[derive(Deserialize)]
struct NodeStatus {
    cpu: f64,
    memory: Usage,
    #[serde(default)]
    swap: Option<Usage>,
    uptime: f64,
    loadavg: Vec<String>,
    rootfs: Usage,
    pveversion: String,
    kversion: String,
}

#[derive(Deserialize)]
struct Usage {
    total: f64,
    used: f64,
}

#[derive(Deserialize)]
struct Guest {
    vmid: u64,
    #[serde(default)]
    name: Option<String>,
    status: String,
    #[serde(default)]
    cpu: Option<f64>,
    #[serde(default)]
    mem: Option<f64>,
    #[serde(default)]
    maxmem: Option<f64>,
    #[serde(default)]
    uptime: Option<f64>,
}

#[derive(Deserialize)]
struct Storage {
    storage: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    active: Option<Flag>,
    #[serde(default)]
    total: Option<f64>,
    #[serde(default)]
    used: Option<f64>,
    #[serde(default)]
    avail: Option<f64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Flag {
    Bool(bool),
    Number(f64),
}

impl Flag {
    fn on(&self) -> bool {
        match self {
            Flag::Bool(b) => *b,
            Flag::Number(n) => *n != 0.0,
        }
    }
}

type Values = Vec<(String, Value)>;

fn number(key: String, v: f64) -> (String, Value) {
    (key, Value::Number(v))
}

fn flag(on: bool) -> f64 {
    if on { 1.0 } else { 0.0 }
}

fn ratio(used: f64, total: f64) -> Option<f64> {
    (total > 0.0).then(|| used / total)
}

struct Host {
    name: String,
    client: reqwest::Client,
    api: reqwest::Url,
}

/// The last `cap` cpu and memory ratios of one node, oldest first.
#[derive(Default)]
struct History {
    cpu: VecDeque<f64>,
    mem: VecDeque<f64>,
}

impl History {
    fn push(&mut self, cap: usize, cpu: f64, mem: f64) {
        for (ring, v) in [(&mut self.cpu, cpu), (&mut self.mem, mem)] {
            ring.push_back(v);
            while ring.len() > cap {
                ring.pop_front();
            }
        }
    }
}

/// What one node contributed to a poll.
#[derive(Default)]
struct NodeReport {
    values: Values,
    online: bool,
    guests_running: usize,
    guests_total: usize,
    failed: usize,
}

/// What one host contributed: its nodes' reports plus the node keys it listed.
struct HostReport {
    nodes: Vec<String>,
    reports: Vec<NodeReport>,
}

pub struct Proxmox {
    name: String,
    interval: Duration,
    history: usize,
    guests: bool,
    storage: bool,
    hosts: Vec<Host>,
    rings: Mutex<HashMap<String, History>>,
    seen: Mutex<HashMap<String, Vec<String>>>,
}

impl Proxmox {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(t) => crate::period::seconds(t, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        anyhow::ensure!(!settings.hosts.is_empty(), "hosts must list at least one Proxmox host");
        let hosts = settings
            .hosts
            .into_iter()
            .map(|h| {
                let auth = format!("PVEAPIToken={}={}", h.token_id, h.token_secret);
                let headers = [("Authorization".to_owned(), auth)].into();
                let base = crate::http::base_url(&h.url).map_err(|e| e.context(format!("host {:?}", h.name)))?;
                Ok(Host {
                    client: crate::http::client(timeout, &headers, None, h.insecure)
                        .map_err(|e| e.context(format!("host {:?}", h.name)))?,
                    api: crate::http::join(&base, "api2/json"),
                    name: h.name,
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            history: settings.history,
            guests: settings.guests,
            storage: settings.storage,
            hosts,
            rings: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
        })
    }

    async fn list<T: for<'de> Deserialize<'de>>(&self, host: &Host, path: &str) -> anyhow::Result<T> {
        let body: Data<T> = crate::http::get(&host.client, crate::http::join(&host.api, path)).await?.json().await?;
        Ok(body.data)
    }

    async fn poll_host(&self, host: &Host) -> anyhow::Result<HostReport> {
        let entries: Vec<NodeEntry> = self.list(host, "nodes").await?;
        let reports = futures::future::join_all(entries.iter().map(|e| self.poll_node(host, e))).await;
        Ok(HostReport { nodes: entries.iter().map(|e| crate::key::sanitize(&e.node)).collect(), reports })
    }

    async fn poll_node(&self, host: &Host, entry: &NodeEntry) -> NodeReport {
        let key = crate::key::sanitize(&entry.node);
        let mut report = NodeReport::default();
        if entry.status != "online" {
            report.values.push(number(format!("node/{key}/online"), 0.0));
            return report;
        }
        let node = &entry.node;
        let optional = |wanted: bool, path: String| async move {
            if wanted { Some((self.list::<serde_json::Value>(host, &path).await, path)) } else { None }
        };
        let status_path = format!("nodes/{node}/status");
        let (status, qemu, lxc, storage) = tokio::join!(
            self.list::<NodeStatus>(host, &status_path),
            optional(self.guests, format!("nodes/{node}/qemu")),
            optional(self.guests, format!("nodes/{node}/lxc")),
            optional(self.storage, format!("nodes/{node}/storage")),
        );
        match status {
            Ok(status) => {
                report.online = true;
                report.values.extend(self.status_values(&key, &status));
            }
            Err(e) => self.sub_failure(host, &status_path, &e, &mut report),
        }
        report.values.push(number(format!("node/{key}/online"), flag(report.online)));
        let mut guests_seen = false;
        for (kind, result) in [("qemu", qemu), ("lxc", lxc)] {
            let Some((result, path)) = result else { continue };
            match result.and_then(|v| Ok(serde_json::from_value::<Vec<Guest>>(v)?)) {
                Ok(guests) => {
                    guests_seen = true;
                    for guest in &guests {
                        report.guests_total += 1;
                        report.guests_running += usize::from(guest.status == "running");
                        report.values.extend(guest_values(kind, node, guest));
                    }
                }
                Err(e) => self.sub_failure(host, &path, &e, &mut report),
            }
        }
        if guests_seen {
            report.values.push(number(format!("node/{key}/guests_running"), report.guests_running as f64));
            report.values.push(number(format!("node/{key}/guests_total"), report.guests_total as f64));
        }
        if let Some((result, path)) = storage {
            match result.and_then(|v| Ok(serde_json::from_value::<Vec<Storage>>(v)?)) {
                Ok(list) => report.values.extend(list.iter().flat_map(|s| storage_values(&key, s))),
                Err(e) => self.sub_failure(host, &path, &e, &mut report),
            }
        }
        report
    }

    fn sub_failure(&self, host: &Host, path: &str, error: &anyhow::Error, report: &mut NodeReport) {
        tracing::warn!(plugin = %self.name, host = %host.name, path, error = format!("{error:#}"), "request failed");
        report.failed += 1;
    }

    fn status_values(&self, key: &str, s: &NodeStatus) -> Values {
        let k = |name: &str| format!("node/{key}/{name}");
        let mem = ratio(s.memory.used, s.memory.total).unwrap_or(0.0);
        let mut out = vec![
            number(k("cpu"), s.cpu),
            number(k("mem"), mem),
            number(k("mem_used_bytes"), s.memory.used),
            number(k("mem_total_bytes"), s.memory.total),
            number(k("uptime"), s.uptime),
            (k("version"), Value::Text(s.pveversion.clone())),
            (k("kernel"), Value::Text(s.kversion.clone())),
        ];
        if let Some(swap) = s.swap.as_ref().and_then(|w| ratio(w.used, w.total)) {
            out.push(number(k("swap"), swap));
        }
        if let Some(load1) = s.loadavg.first().and_then(|l| l.trim().parse().ok()) {
            out.push(number(k("load1"), load1));
        }
        if let Some(rootfs) = ratio(s.rootfs.used, s.rootfs.total) {
            out.push(number(k("rootfs"), rootfs));
        }
        if self.history > 0 {
            let mut rings = self.rings.lock().expect("history rings poisoned");
            let ring = rings.entry(key.to_owned()).or_default();
            ring.push(self.history, s.cpu, mem);
            out.push((k("cpu_history"), Value::Series(ring.cpu.iter().copied().collect())));
            out.push((k("mem_history"), Value::Series(ring.mem.iter().copied().collect())));
        }
        out
    }
}

fn guest_values(kind: &str, node: &str, g: &Guest) -> Values {
    let name = g.name.as_deref().map_or_else(|| format!("vm{}", g.vmid), crate::key::sanitize);
    let k = |field: &str| format!("guest/{name}/{field}");
    let mut out = vec![
        number(k("status"), flag(g.status == "running")),
        (k("type"), Value::Text(kind.to_owned())),
        number(k("vmid"), g.vmid as f64),
        (k("node"), Value::Text(node.to_owned())),
    ];
    if let Some(cpu) = g.cpu {
        out.push(number(k("cpu"), cpu));
    }
    if let Some(mem) = g.mem {
        out.push(number(k("mem_bytes"), mem));
        if let Some(r) = g.maxmem.and_then(|max| ratio(mem, max)) {
            out.push(number(k("mem"), r));
        }
    }
    if let Some(uptime) = g.uptime {
        out.push(number(k("uptime"), uptime));
    }
    out
}

fn storage_values(node: &str, s: &Storage) -> Values {
    let id = crate::key::sanitize(&s.storage);
    let k = |field: &str| format!("storage/{node}/{id}/{field}");
    let mut out = vec![
        number(k("active"), flag(s.active.as_ref().is_some_and(Flag::on))),
        (k("type"), Value::Text(s.kind.clone())),
    ];
    if let (Some(used), Some(total)) = (s.used, s.total)
        && let Some(r) = ratio(used, total)
    {
        out.push(number(k("used"), r));
    }
    if let Some(avail) = s.avail {
        out.push(number(k("avail_bytes"), avail));
    }
    if let Some(total) = s.total {
        out.push(number(k("total_bytes"), total));
    }
    out
}

#[async_trait]
impl Plugin for Proxmox {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let results = futures::future::join_all(self.hosts.iter().map(|h| self.poll_host(h))).await;
        let mut partial = crate::partial::Partial::new(&self.name, "hosts");
        let mut extra = Values::new();
        let (mut nodes_online, mut nodes_total, mut running, mut total, mut hosts_failed, mut failed) =
            (0, 0, 0, 0, 0, 0);
        let mut seen = self.seen.lock().expect("seen nodes poisoned");
        for (host, result) in self.hosts.iter().zip(results) {
            match result {
                Ok(report) => {
                    nodes_total += report.nodes.len();
                    let mut values = Values::new();
                    for node in report.reports {
                        nodes_online += usize::from(node.online);
                        running += node.guests_running;
                        total += node.guests_total;
                        failed += node.failed;
                        values.extend(node.values);
                    }
                    seen.insert(host.name.clone(), report.nodes);
                    partial.ok(values);
                }
                Err(e) => {
                    hosts_failed += 1;
                    partial.fail(&host.name, &e);
                    for node in seen.get(&host.name).into_iter().flatten() {
                        nodes_total += 1;
                        extra.push(number(format!("node/{node}/online"), 0.0));
                    }
                }
            }
        }
        drop(seen);
        let mut values = partial.finish()?;
        values.extend(extra);
        let guests = [("guests_running", running), ("guests_total", total)];
        let totals = [
            ("nodes_online", nodes_online),
            ("nodes_total", nodes_total),
            ("hosts_failed", hosts_failed),
            ("_requests_failed", failed),
        ];
        for (key, v) in totals.into_iter().chain(guests.into_iter().filter(|_| self.guests)) {
            values.push(number(key.to_owned(), v as f64));
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;

    const SECRET: &str = "5f0c2a8e-1111-2222-3333-444444444444";
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    fn data(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "data": body }))
    }

    fn node_entry(node: &str, status: &str) -> serde_json::Value {
        if status != "online" {
            return json!({"node": node, "status": status, "type": "node", "id": format!("node/{node}"), "level": ""});
        }
        json!({"node": node, "status": status, "type": "node", "id": format!("node/{node}"), "level": "",
               "cpu": 0.05, "maxcpu": 16, "mem": 8 * 1024_u64.pow(3), "maxmem": 32 * 1024_u64.pow(3),
               "uptime": 86400, "ssl_fingerprint": "AA:BB"})
    }

    fn node_status(cpu: f64, swap_total: u64) -> serde_json::Value {
        json!({
            "uptime": 123456, "idle": 0, "wait": 0.001, "cpu": cpu,
            "loadavg": ["0.42", "0.50", "0.61"],
            "kversion": "Linux 6.8.12-4-pve #1 SMP PREEMPT_DYNAMIC PMX 6.8.12-4 (2024-11-06T15:04Z)",
            "current-kernel": {"sysname": "Linux", "release": "6.8.12-4-pve", "machine": "x86_64",
                               "version": "#1 SMP PREEMPT_DYNAMIC PMX 6.8.12-4 (2024-11-06T15:04Z)"},
            "boot-info": {"mode": "efi", "secureboot": 0},
            "cpuinfo": {"model": "AMD Ryzen 7 5700G", "cpus": 16, "sockets": 1, "cores": 8, "mhz": "3800.000"},
            "memory": {"total": 32 * 1024_u64.pow(3), "used": 8 * 1024_u64.pow(3), "free": 24 * 1024_u64.pow(3),
                       "available": 23 * 1024_u64.pow(3)},
            "ksm": {"shared": 0},
            "swap": {"total": swap_total, "used": swap_total / 4, "free": swap_total - swap_total / 4},
            "pveversion": "pve-manager/8.3.0/c1689ccb1065a83b",
            "rootfs": {"total": 100 * 1024_u64.pow(3), "used": 25 * 1024_u64.pow(3),
                       "avail": 75 * 1024_u64.pow(3), "free": 75 * 1024_u64.pow(3)}
        })
    }

    fn qemu() -> serde_json::Value {
        json!([
            {"vmid": 611, "name": "cddlabs-w1", "status": "running", "cpu": 0.25, "cpus": 4,
             "mem": 4 * 1024_u64.pow(3), "maxmem": 8 * 1024_u64.pow(3), "maxdisk": 107374182400_u64,
             "disk": 0, "diskread": 1, "diskwrite": 2, "netin": 3, "netout": 4, "uptime": 3600,
             "pid": 1234, "qmpstatus": "running", "template": 0, "tags": "k8s"},
            {"vmid": 9000, "status": "stopped", "cpu": 0, "cpus": 2, "mem": 0, "maxmem": 2 * 1024_u64.pow(3),
             "uptime": 0, "template": 1}
        ])
    }

    fn lxc() -> serde_json::Value {
        json!([
            {"vmid": 153, "name": "dns-01", "status": "running", "type": "lxc", "cpu": 0.01, "cpus": 2,
             "mem": 256 * 1024_u64.pow(2), "maxmem": 1024 * 1024_u64.pow(2), "maxswap": 0, "disk": 1,
             "maxdisk": 2, "uptime": 7200}
        ])
    }

    fn storage() -> serde_json::Value {
        json!([
            {"storage": "local-zfs", "type": "zfspool", "content": "images,rootdir", "active": 1, "enabled": 1,
             "shared": 0, "total": 400 * 1024_u64.pow(3), "used": 100 * 1024_u64.pow(3),
             "avail": 300 * 1024_u64.pow(3), "used_fraction": 0.25},
            {"storage": "nas-backup", "type": "nfs", "content": "backup", "active": 0, "enabled": 0, "shared": 1}
        ])
    }

    async fn mount(server: &MockServer, route: &str, token_id: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/api2/json/{route}")))
            .and(header("authorization", format!("PVEAPIToken={token_id}={SECRET}")))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn host(node: &str) -> MockServer {
        let server = MockServer::start().await;
        let token = "peephole@pve!ro";
        mount(&server, "nodes", token, data(json!([node_entry(node, "online")]))).await;
        mount(&server, &format!("nodes/{node}/status"), token, data(node_status(0.125, 8 * 1024_u64.pow(3)))).await;
        mount(&server, &format!("nodes/{node}/qemu"), token, data(qemu())).await;
        mount(&server, &format!("nodes/{node}/lxc"), token, data(lxc())).await;
        mount(&server, &format!("nodes/{node}/storage"), token, data(storage())).await;
        server
    }

    fn host_yaml(name: &str, server: &MockServer) -> String {
        format!("{{name: {name}, url: '{}', token_id: 'peephole@pve!ro', token_secret: '{SECRET}'}}", server.uri())
    }

    fn plugin(hosts: &[String], extra: &str) -> Proxmox {
        let yaml = format!("hosts: [{}]\n{extra}", hosts.join(", "));
        Proxmox::new("pve", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Proxmox) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    fn n(got: &BTreeMap<String, Value>, k: &str) -> f64 {
        got.get(k).and_then(Value::as_f64).unwrap_or_else(|| panic!("{k} is not a number: {:?}", got.get(k)))
    }

    fn t(got: &BTreeMap<String, Value>, k: &str) -> String {
        got.get(k).and_then(Value::as_text).unwrap_or_else(|| panic!("{k} is not text: {:?}", got.get(k))).to_owned()
    }

    #[tokio::test]
    async fn reads_node_status_guests_and_storage() {
        let a = host("cdd-prox-01").await;
        let got = poll(&plugin(&[host_yaml("pve-a", &a)], "")).await;
        let node = |k: &str| format!("node/cdd-prox-01/{k}");
        assert_eq!(n(&got, &node("cpu")), 0.125);
        assert_eq!(n(&got, &node("mem")), 0.25);
        assert_eq!(n(&got, &node("mem_used_bytes")), 8.0 * GIB);
        assert_eq!(n(&got, &node("mem_total_bytes")), 32.0 * GIB);
        assert_eq!(n(&got, &node("swap")), 0.25);
        assert_eq!(n(&got, &node("uptime")), 123456.0);
        assert_eq!(n(&got, &node("load1")), 0.42);
        assert_eq!(n(&got, &node("rootfs")), 0.25);
        assert_eq!(t(&got, &node("version")), "pve-manager/8.3.0/c1689ccb1065a83b");
        assert!(t(&got, &node("kernel")).starts_with("Linux 6.8.12-4-pve"));
        assert_eq!(n(&got, &node("online")), 1.0);
        assert_eq!(n(&got, &node("guests_running")), 2.0);
        assert_eq!(n(&got, &node("guests_total")), 3.0);

        assert_eq!(n(&got, "guest/cddlabs-w1/status"), 1.0);
        assert_eq!(t(&got, "guest/cddlabs-w1/type"), "qemu");
        assert_eq!(n(&got, "guest/cddlabs-w1/vmid"), 611.0);
        assert_eq!(t(&got, "guest/cddlabs-w1/node"), "cdd-prox-01");
        assert_eq!(n(&got, "guest/cddlabs-w1/cpu"), 0.25);
        assert_eq!(n(&got, "guest/cddlabs-w1/mem"), 0.5);
        assert_eq!(n(&got, "guest/cddlabs-w1/mem_bytes"), 4.0 * GIB);
        assert_eq!(n(&got, "guest/cddlabs-w1/uptime"), 3600.0);
        assert_eq!(n(&got, "guest/vm9000/status"), 0.0);
        assert_eq!(n(&got, "guest/vm9000/mem"), 0.0);
        assert_eq!(t(&got, "guest/dns-01/type"), "lxc");
        assert_eq!(n(&got, "guest/dns-01/mem"), 0.25);

        assert_eq!(n(&got, "storage/cdd-prox-01/local-zfs/used"), 0.25);
        assert_eq!(n(&got, "storage/cdd-prox-01/local-zfs/avail_bytes"), 300.0 * GIB);
        assert_eq!(n(&got, "storage/cdd-prox-01/local-zfs/total_bytes"), 400.0 * GIB);
        assert_eq!(n(&got, "storage/cdd-prox-01/local-zfs/active"), 1.0);
        assert_eq!(t(&got, "storage/cdd-prox-01/local-zfs/type"), "zfspool");
        assert_eq!(n(&got, "storage/cdd-prox-01/nas-backup/active"), 0.0);
        assert_eq!(t(&got, "storage/cdd-prox-01/nas-backup/type"), "nfs");
        assert!(!got.contains_key("storage/cdd-prox-01/nas-backup/used"));
        assert!(!got.contains_key("storage/cdd-prox-01/nas-backup/total_bytes"));

        assert_eq!(n(&got, "nodes_online"), 1.0);
        assert_eq!(n(&got, "nodes_total"), 1.0);
        assert_eq!(n(&got, "guests_running"), 2.0);
        assert_eq!(n(&got, "guests_total"), 3.0);
        assert_eq!(n(&got, "hosts_failed"), 0.0);
        assert_eq!(n(&got, "_hosts_ok"), 1.0);
        assert_eq!(n(&got, "_requests_failed"), 0.0);
        assert!(!got.values().any(|v| v.as_text().is_some_and(|s| s.contains(SECRET))));
    }

    #[tokio::test]
    async fn two_hosts_add_up() {
        let (a, b) = (host("pve-a").await, host("pve-b").await);
        let got = poll(&plugin(&[host_yaml("a", &a), host_yaml("b", &b)], "")).await;
        assert_eq!(n(&got, "node/pve-a/online"), 1.0);
        assert_eq!(n(&got, "node/pve-b/online"), 1.0);
        assert_eq!(n(&got, "nodes_online"), 2.0);
        assert_eq!(n(&got, "nodes_total"), 2.0);
        assert_eq!(n(&got, "guests_total"), 6.0);
        assert_eq!(n(&got, "guests_running"), 4.0);
        assert_eq!(n(&got, "_hosts_ok"), 2.0);
    }

    #[tokio::test]
    async fn a_failing_host_leaves_the_other_and_its_seen_nodes_count_as_offline() {
        let (a, b) = (host("pve-a").await, host("pve-b").await);
        let never = MockServer::start().await;
        let p = plugin(&[host_yaml("a", &a), host_yaml("b", &b), host_yaml("never", &never)], "");
        let first = poll(&p).await;
        assert_eq!(n(&first, "nodes_total"), 2.0);
        assert_eq!(n(&first, "hosts_failed"), 1.0);
        b.reset().await;
        let got = poll(&p).await;
        assert_eq!(n(&got, "node/pve-a/online"), 1.0);
        assert_eq!(n(&got, "node/pve-b/online"), 0.0);
        assert!(!got.contains_key("node/pve-b/cpu"));
        assert_eq!(n(&got, "nodes_online"), 1.0);
        assert_eq!(n(&got, "nodes_total"), 2.0);
        assert_eq!(n(&got, "guests_total"), 3.0);
        assert_eq!(n(&got, "hosts_failed"), 2.0);
        assert_eq!(n(&got, "_hosts_ok"), 1.0);
        assert_eq!(n(&got, "_hosts_failed"), 2.0);
    }

    #[tokio::test]
    async fn every_host_failing_is_an_error_naming_them_without_the_secret() {
        let (a, b) = (MockServer::start().await, MockServer::start().await);
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&a).await;
        let err =
            format!("{:#}", plugin(&[host_yaml("pve-a", &a), host_yaml("pve-b", &b)], "").poll().await.unwrap_err());
        assert!(err.contains("pve-a") && err.contains("pve-b") && err.contains("401"), "{err}");
        assert!(!err.contains(SECRET), "{err}");
    }

    #[tokio::test]
    async fn a_failing_guest_or_storage_request_keeps_the_node() {
        let server = MockServer::start().await;
        let token = "peephole@pve!ro";
        mount(&server, "nodes", token, data(json!([node_entry("pve-a", "online")]))).await;
        mount(&server, "nodes/pve-a/status", token, data(node_status(0.5, 0))).await;
        mount(&server, "nodes/pve-a/lxc", token, data(lxc())).await;
        mount(&server, "nodes/pve-a/storage", token, ResponseTemplate::new(500)).await;
        let got = poll(&plugin(&[host_yaml("a", &server)], "")).await;
        assert_eq!(n(&got, "node/pve-a/cpu"), 0.5);
        assert!(!got.contains_key("node/pve-a/swap"));
        assert_eq!(n(&got, "node/pve-a/guests_total"), 1.0);
        assert_eq!(n(&got, "guest/dns-01/status"), 1.0);
        assert!(!got.keys().any(|k| k.starts_with("storage/")));
        assert_eq!(n(&got, "_requests_failed"), 2.0);
        assert_eq!(n(&got, "_hosts_ok"), 1.0);
    }

    #[tokio::test]
    async fn a_cluster_endpoint_reports_offline_members_without_asking_them() {
        let server = MockServer::start().await;
        let token = "peephole@pve!ro";
        let nodes = json!([node_entry("pve-a", "online"), node_entry("pve-c", "offline")]);
        mount(&server, "nodes", token, data(nodes)).await;
        mount(&server, "nodes/pve-a/status", token, data(node_status(0.5, 0))).await;
        Mock::given(path("/api2/json/nodes/pve-c/status"))
            .respond_with(ResponseTemplate::new(595))
            .expect(0)
            .mount(&server)
            .await;
        let got = poll(&plugin(&[host_yaml("cluster", &server)], "guests: false\nstorage: false")).await;
        assert_eq!(n(&got, "node/pve-a/online"), 1.0);
        assert_eq!(n(&got, "node/pve-c/online"), 0.0);
        assert_eq!(n(&got, "nodes_online"), 1.0);
        assert_eq!(n(&got, "nodes_total"), 2.0);
        assert!(!got.keys().any(|k| k.starts_with("guest/") || k.starts_with("storage/") || k.contains("guests_")));
        assert_eq!(n(&got, "_requests_failed"), 0.0);
    }

    struct Rising(AtomicUsize);

    impl Respond for Rising {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let i = self.0.fetch_add(1, Ordering::SeqCst) as f64;
            data(node_status(i / 10.0, 0))
        }
    }

    #[tokio::test]
    async fn history_keeps_the_last_samples_oldest_first() {
        let server = MockServer::start().await;
        let token = "peephole@pve!ro";
        mount(&server, "nodes", token, data(json!([node_entry("pve-a", "online")]))).await;
        Mock::given(path("/api2/json/nodes/pve-a/status"))
            .respond_with(Rising(AtomicUsize::new(0)))
            .mount(&server)
            .await;
        let p = plugin(&[host_yaml("a", &server)], "history: 3\nguests: false\nstorage: false");
        let first = poll(&p).await;
        assert_eq!(first["node/pve-a/cpu_history"], Value::Series(vec![0.0]));
        for _ in 0..3 {
            p.poll().await.unwrap();
        }
        let got = poll(&p).await;
        assert_eq!(got["node/pve-a/cpu_history"], Value::Series(vec![0.2, 0.3, 0.4]));
        assert_eq!(got["node/pve-a/mem_history"], Value::Series(vec![0.25, 0.25, 0.25]));
        let off = plugin(&[host_yaml("a", &server)], "history: 0\nguests: false\nstorage: false");
        assert!(!poll(&off).await.keys().any(|k| k.ends_with("_history")));
    }

    #[test]
    fn settings_defaults_and_validation() {
        let one = "[{name: a, url: 'https://pve-a.example.invalid:8006', token_id: 'u@pve!t', token_secret: s}]";
        let s: Settings = serde_yaml::from_str(&format!("hosts: {one}")).unwrap();
        assert_eq!((s.interval_s, s.timeout_s, s.history, s.guests, s.storage), (30, None, 60, true, true));
        assert!(!s.hosts[0].insecure);
        assert_eq!(Proxmox::new("p", s).unwrap().interval(), Duration::from_secs(30));
        let cases = [
            ("hosts: []".to_owned(), "hosts"),
            (format!("hosts: {one}\ninterval_s: 0"), "interval_s"),
            (format!("hosts: {one}\ntimeout_s: 0"), "timeout_s"),
            ("hosts: [{name: a, url: 'not a url', token_id: 'u@pve!t', token_secret: s}]".to_owned(), "not a url"),
            (
                "hosts: [{name: a, url: 'ftp://pve.example.invalid', token_id: 'u@pve!t', token_secret: s}]".to_owned(),
                "http",
            ),
        ];
        for (yaml, needle) in cases {
            let err = format!("{:#}", Proxmox::new("p", serde_yaml::from_str(&yaml).unwrap()).err().unwrap());
            assert!(err.contains(needle), "{yaml}: {err}");
        }
        let unknown = format!("hosts: {one}\nnodes: 3");
        assert!(serde_yaml::from_str::<Settings>(&unknown).is_err());
    }
}
