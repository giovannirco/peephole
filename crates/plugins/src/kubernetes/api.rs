//! The fields Peephole reads from each API object; everything else is ignored.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Any `…List` response. `metadata.continue` is set when more pages remain.
#[derive(Deserialize)]
pub(super) struct List<T> {
    pub items: Vec<T>,
    #[serde(default)]
    pub metadata: ListMeta,
}

#[derive(Deserialize, Default)]
pub(super) struct ListMeta {
    #[serde(default, rename = "continue")]
    pub next: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Meta {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
}

#[derive(Deserialize)]
pub(super) struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The `Ready` condition, when the object has one.
pub(super) fn ready(conditions: &[Condition]) -> Option<&Condition> {
    conditions.iter().find(|c| c.kind == "Ready")
}

pub(super) fn is_ready(conditions: &[Condition]) -> bool {
    ready(conditions).is_some_and(|c| c.status == "True")
}

/// `GET /version`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Version {
    pub git_version: String,
}

#[derive(Deserialize)]
pub(super) struct Node {
    pub metadata: Meta,
    #[serde(default)]
    pub status: NodeStatus,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct NodeStatus {
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub allocatable: BTreeMap<String, String>,
    #[serde(default)]
    pub node_info: Option<NodeInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NodeInfo {
    pub kubelet_version: String,
}

/// `metrics.k8s.io/v1beta1` NodeMetrics.
#[derive(Deserialize)]
pub(super) struct NodeMetrics {
    pub metadata: Meta,
    #[serde(default)]
    pub usage: BTreeMap<String, String>,
}

#[derive(Deserialize)]
pub(super) struct Pod {
    pub metadata: Meta,
    #[serde(default)]
    pub status: PodStatus,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct PodStatus {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub container_statuses: Vec<ContainerStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ContainerStatus {
    #[serde(default)]
    pub restart_count: u64,
}

#[derive(Deserialize)]
pub(super) struct Pvc {
    #[serde(default)]
    pub status: PhaseStatus,
}

#[derive(Deserialize, Default)]
pub(super) struct PhaseStatus {
    #[serde(default)]
    pub phase: String,
}

/// Core `v1` Event. `lastTimestamp` is null for events written through
/// `events.k8s.io`, which set `eventTime` and `series.lastObservedTime` instead.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Event {
    pub metadata: Meta,
    pub involved_object: ObjectReference,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub last_timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub event_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub series: Option<EventSeries>,
}

#[derive(Deserialize)]
pub(super) struct ObjectReference {
    #[serde(default)]
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EventSeries {
    #[serde(default)]
    pub last_observed_time: Option<DateTime<Utc>>,
}

impl Event {
    /// The latest of the three timestamps an event may carry.
    pub fn last_seen(&self) -> Option<DateTime<Utc>> {
        let series = self.series.as_ref().and_then(|s| s.last_observed_time);
        [self.last_timestamp, self.event_time, series].into_iter().flatten().max()
    }
}

/// Argo CD `Application`: `status.sync.status` is Synced, OutOfSync or Unknown;
/// `status.health.status` is Healthy, Progressing, Degraded, Suspended, Missing or Unknown.
#[derive(Deserialize)]
pub(super) struct Application {
    pub metadata: Meta,
    #[serde(default)]
    pub status: ApplicationStatus,
}

#[derive(Deserialize, Default)]
pub(super) struct ApplicationStatus {
    #[serde(default)]
    pub sync: StatusField,
    #[serde(default)]
    pub health: StatusField,
}

#[derive(Deserialize, Default)]
pub(super) struct StatusField {
    #[serde(default)]
    pub status: String,
}

/// A Flux Kustomization or HelmRelease: only its conditions matter.
#[derive(Deserialize)]
pub(super) struct Reconciled {
    pub metadata: Meta,
    #[serde(default)]
    pub status: ConditionsStatus,
}

#[derive(Deserialize, Default)]
pub(super) struct ConditionsStatus {
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// cert-manager `Certificate`.
#[derive(Deserialize)]
pub(super) struct Certificate {
    pub metadata: Meta,
    #[serde(default)]
    pub status: CertificateStatus,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct CertificateStatus {
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub not_after: Option<DateTime<Utc>>,
}
