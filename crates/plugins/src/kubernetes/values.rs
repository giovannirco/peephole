//! Pure functions from parsed API objects to store values (keys without the cluster prefix).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use peephole_core::Value;

use super::api;
use super::quantity;
use crate::key::sanitize;

pub(super) type Values = Vec<(String, Value)>;

const WARNING_WINDOW_S: i64 = 3600;

fn num(key: impl Into<String>, n: f64) -> (String, Value) {
    (key.into(), Value::Number(n))
}

fn text(key: impl Into<String>, t: impl Into<String>) -> (String, Value) {
    (key.into(), Value::Text(t.into()))
}

fn flag(key: impl Into<String>, on: bool) -> (String, Value) {
    num(key, if on { 1.0 } else { 0.0 })
}

fn count<T>(items: &[T], keep: impl Fn(&T) -> bool) -> f64 {
    items.iter().filter(|i| keep(i)).count() as f64
}

pub(super) fn version(v: api::Version) -> Values {
    vec![text("version", v.git_version)]
}

fn allocatable(node: &api::Node, resource: &str) -> anyhow::Result<Option<f64>> {
    node.status
        .allocatable
        .get(resource)
        .map(|q| quantity::parse(q))
        .transpose()
        .map_err(|e| anyhow::anyhow!("node {}: allocatable {resource}: {e}", node.metadata.name))
}

pub(super) fn nodes(nodes: &[api::Node]) -> anyhow::Result<Values> {
    let mut out = vec![
        num("nodes_total", nodes.len() as f64),
        num("nodes_ready", count(nodes, |n| api::is_ready(&n.status.conditions))),
    ];
    for node in nodes {
        let key = format!("node/{}", sanitize(&node.metadata.name));
        out.push(flag(format!("{key}/ready"), api::is_ready(&node.status.conditions)));
        if let Some(info) = &node.status.node_info {
            out.push(text(format!("{key}/version"), info.kubelet_version.clone()));
        }
        for (resource, suffix) in [("cpu", "cpu_alloc"), ("memory", "mem_alloc_bytes"), ("pods", "pods_alloc")] {
            if let Some(v) = allocatable(node, resource)? {
                out.push(num(format!("{key}/{suffix}"), v));
            }
        }
    }
    Ok(out)
}

/// Usage over allocatable per node and cluster-wide; raw cores when the node list is unavailable.
pub(super) fn metrics(usage: &[api::NodeMetrics], nodes: Option<&[api::Node]>) -> anyhow::Result<Values> {
    let mut allocated: BTreeMap<&str, (Option<f64>, Option<f64>)> = BTreeMap::new();
    for node in nodes.unwrap_or_default() {
        allocated.insert(&node.metadata.name, (allocatable(node, "cpu")?, allocatable(node, "memory")?));
    }
    let mut out = Vec::new();
    let (mut cpu_used, mut cpu_alloc, mut mem_used, mut mem_alloc) = (0.0, 0.0, 0.0, 0.0);
    for m in usage {
        let name = &m.metadata.name;
        let key = format!("node/{}", sanitize(name));
        let parse = |resource: &str| {
            m.usage
                .get(resource)
                .map(|q| quantity::parse(q))
                .transpose()
                .map_err(|e| anyhow::anyhow!("node {name}: usage {resource}: {e}"))
        };
        let (cpu, mem) = (parse("cpu")?, parse("memory")?);
        let (alloc_cpu, alloc_mem) = allocated.get(name.as_str()).copied().unwrap_or((None, None));
        match (cpu, alloc_cpu) {
            (Some(used), Some(total)) if total > 0.0 => {
                out.push(num(format!("{key}/cpu"), used / total));
                cpu_used += used;
                cpu_alloc += total;
            }
            (Some(used), _) if nodes.is_none() => out.push(num(format!("{key}/cpu_cores"), used)),
            _ => {}
        }
        if let (Some(used), Some(total)) = (mem, alloc_mem)
            && total > 0.0
        {
            out.push(num(format!("{key}/mem"), used / total));
            mem_used += used;
            mem_alloc += total;
        }
    }
    if cpu_alloc > 0.0 {
        out.push(num("cpu", cpu_used / cpu_alloc));
    }
    if mem_alloc > 0.0 {
        out.push(num("mem", mem_used / mem_alloc));
    }
    Ok(out)
}

pub(super) fn pods(pods: &[api::Pod]) -> Values {
    fn phase(p: &api::Pod) -> &str {
        &p.status.phase
    }
    let bad = |p: &api::Pod| matches!(phase(p), "Pending" | "Failed" | "Unknown");
    let restarts: u64 = pods.iter().flat_map(|p| &p.status.container_statuses).map(|c| c.restart_count).sum();
    let mut per_ns: BTreeMap<&str, usize> = BTreeMap::new();
    for pod in pods {
        *per_ns.entry(&pod.metadata.namespace).or_default() += 1;
    }
    let mut out = vec![
        num("pods_total", pods.len() as f64),
        num("pods_running", count(pods, |p| phase(p) == "Running")),
        num("pods_succeeded", count(pods, |p| phase(p) == "Succeeded")),
        num("pods_bad", count(pods, bad)),
        num("pods_pending", count(pods, |p| phase(p) == "Pending")),
        num("pods_failed", count(pods, |p| phase(p) == "Failed")),
        num("restarts", restarts as f64),
        num("namespaces", per_ns.len() as f64),
    ];
    let mut top: Option<(&str, usize)> = None;
    for (ns, n) in &per_ns {
        out.push(num(format!("ns/{}/pods", sanitize(ns)), *n as f64));
        if top.is_none_or(|(_, best)| *n > best) {
            top = Some((ns, *n));
        }
    }
    if let Some((ns, n)) = top {
        out.push(text("top_ns", ns));
        out.push(num("top_ns_pods", n as f64));
    }
    let bad_pod = pods.iter().find(|p| bad(p)).map(|p| format!("{}/{}", p.metadata.namespace, p.metadata.name));
    out.push(text("bad_pod", bad_pod.unwrap_or_default()));
    out
}

pub(super) fn pvcs(pvcs: &[api::Pvc]) -> Values {
    vec![
        num("pvc_total", pvcs.len() as f64),
        num("pvc_bound", count(pvcs, |p| p.status.phase == "Bound")),
        num("pvc_pending", count(pvcs, |p| p.status.phase == "Pending")),
    ]
}

pub(super) fn events(events: &[api::Event], now: DateTime<Utc>) -> Values {
    let recent = count(events, |e| e.last_seen().is_some_and(|t| (now - t).num_seconds() <= WARNING_WINDOW_S));
    let mut out = vec![num("warnings_1h", recent)];
    let latest = events.iter().filter_map(|e| e.last_seen().map(|t| (t, e))).max_by_key(|(t, _)| *t);
    match latest {
        Some((at, e)) => {
            let reason = e.reason.as_deref().unwrap_or("");
            out.push(text("last_warning", format!("{}/{}: {reason}", e.metadata.namespace, e.involved_object.name)));
            out.push(num("last_warning_age", (now - at).num_seconds().max(0) as f64));
        }
        None => out.push(text("last_warning", "")),
    }
    out
}

pub(super) fn argo(apps: &[api::Application]) -> Values {
    fn sync(a: &api::Application) -> &str {
        &a.status.sync.status
    }
    fn health(a: &api::Application) -> &str {
        &a.status.health.status
    }
    let mut out = vec![
        num("argo_apps", apps.len() as f64),
        num("argo_synced", count(apps, |a| sync(a) == "Synced")),
        num("argo_outofsync", count(apps, |a| sync(a) == "OutOfSync")),
        num("argo_healthy", count(apps, |a| health(a) == "Healthy")),
        num("argo_degraded", count(apps, |a| health(a) == "Degraded")),
        num("argo_progressing", count(apps, |a| health(a) == "Progressing")),
        num("argo_missing", count(apps, |a| health(a) == "Missing")),
    ];
    let mut worst = String::new();
    for app in apps {
        let key = format!("argo/{}", sanitize(&app.metadata.name));
        out.push(flag(format!("{key}/synced"), sync(app) == "Synced"));
        out.push(flag(format!("{key}/healthy"), health(app) == "Healthy"));
        out.push(text(format!("{key}/health"), health(app)));
        if worst.is_empty() {
            if health(app) != "Healthy" {
                worst = format!("{}: {}", app.metadata.name, health(app));
            } else if sync(app) != "Synced" {
                worst = format!("{}: {}", app.metadata.name, sync(app));
            }
        }
    }
    out.push(text("argo_worst", worst));
    out
}

/// Per-object readiness of one Flux kind (`kustomization`, `helmrelease`).
pub(super) fn flux_objects(kind: &str, objects: &[api::Reconciled]) -> Values {
    objects
        .iter()
        .map(|o| flag(format!("flux/{kind}/{}/ready", sanitize(&o.metadata.name)), api::is_ready(&o.status.conditions)))
        .collect()
}

/// Totals over every Flux kind that answered.
pub(super) fn flux_totals(kinds: &[(&str, &[api::Reconciled])]) -> Values {
    let all: Vec<(&str, &api::Reconciled)> =
        kinds.iter().flat_map(|(kind, objects)| objects.iter().map(move |o| (*kind, o))).collect();
    let ready = all.iter().filter(|(_, o)| api::is_ready(&o.status.conditions)).count();
    let worst = all.iter().find(|(_, o)| !api::is_ready(&o.status.conditions)).map(|(kind, o)| {
        let reason = api::ready(&o.status.conditions).and_then(|c| c.reason.as_deref()).unwrap_or("Unknown");
        format!("{kind}/{}/{}: {reason}", o.metadata.namespace, o.metadata.name)
    });
    vec![
        num("flux_total", all.len() as f64),
        num("flux_ready", ready as f64),
        num("flux_notready", (all.len() - ready) as f64),
        text("flux_worst", worst.unwrap_or_default()),
    ]
}

/// Whole days until expiry, rounded down (an expired certificate is negative).
fn days_left(not_after: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    (not_after - now).num_seconds().div_euclid(86_400) as f64
}

pub(super) fn certificates(certs: &[api::Certificate], now: DateTime<Utc>) -> Values {
    let ready = count(certs, |c| api::is_ready(&c.status.conditions));
    let mut out = vec![
        num("certs_total", certs.len() as f64),
        num("certs_ready", ready),
        num("certs_notready", certs.len() as f64 - ready),
    ];
    let mut soonest: Option<(DateTime<Utc>, &api::Certificate)> = None;
    for cert in certs {
        let key = format!("cert/{}", sanitize(&cert.metadata.name));
        out.push(flag(format!("{key}/ready"), api::is_ready(&cert.status.conditions)));
        if let Some(not_after) = cert.status.not_after {
            out.push(num(format!("{key}/days"), days_left(not_after, now)));
            if soonest.is_none_or(|(best, _)| not_after < best) {
                soonest = Some((not_after, cert));
            }
        }
    }
    if let Some((not_after, cert)) = soonest {
        out.push(num("cert_min_days", days_left(not_after, now)));
        out.push(text("cert_soonest", format!("{}/{}", cert.metadata.namespace, cert.metadata.name)));
    }
    out
}
