//! job-scout (`giovannirco/job-scout`): aggregate counts only, never a company,
//! title, slug, person or URL.
//!
//! Endpoints, under `/api/v1` with `Authorization: Bearer <token>`, each answering
//! the envelope `{"ok": true, "data": …, "meta": {"total": …}}` or
//! `{"ok": false, "error": {"code": …}}`: `GET /today`, `GET /radar/discovery/summary?hours=24`,
//! `GET /processes`, `GET /leads?lane=<lane>&pageSize=1` (`meta.total`). Verified against
//! job-scout's `docs/API.md` and the handlers `apps/api/src/routes/{today,radar,process,leads}.ts`
//! with `packages/core/src/{funnel,llm,radar,interviews,leads}.ts` (job-scout 2.14.4).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use peephole_core::{Plugin, Value};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// No `Debug`: the struct holds the API token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Base URL of the job-scout server (the part before `/api/v1`).
    pub url: String,
    /// API token, sent as a bearer token; never logged.
    pub token: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub insecure: bool,
}

fn default_interval() -> u64 {
    300
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    data: Option<T>,
    #[serde(default)]
    meta: Meta,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Default, Deserialize)]
struct Meta {
    #[serde(default)]
    total: Option<f64>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
}

/// `GET /today`, reduced to its counts.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Today {
    decisions: Vec<serde::de::IgnoredAny>,
    follow_ups: Vec<serde::de::IgnoredAny>,
    upcoming: Vec<Round>,
    counts: Counts,
    queue: Vec<QueueRow>,
    llm: Llm,
    approvals: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Round {
    scheduled_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Counts {
    by_status: BTreeMap<String, f64>,
    untriaged: f64,
    last30d: BTreeMap<String, f64>,
    applied_this_week: f64,
}

#[derive(Deserialize)]
struct QueueRow {
    status: String,
    c: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Llm {
    by_operation: Vec<LlmOperation>,
    today: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LlmOperation {
    failures: f64,
    tokens_in: f64,
    tokens_out: f64,
}

/// `GET /radar/discovery/summary`.
#[derive(Deserialize)]
struct Discovery {
    lanes: Vec<Lane>,
    boards: Boards,
}

#[derive(Deserialize)]
struct Lane {
    lane: String,
    c: f64,
}

#[derive(Deserialize)]
struct Boards {
    enabled: f64,
    errored: f64,
}

/// One live loop of `GET /processes`; only its status is read.
#[derive(Deserialize)]
struct Process {
    status: String,
}

type Values = Vec<(String, Value)>;

fn number(key: impl Into<String>, n: f64) -> (String, Value) {
    (key.into(), Value::Number(n))
}

pub struct JobScout {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    api: reqwest::Url,
}

impl JobScout {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(t) => crate::period::seconds(t, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        let base = crate::http::base_url(&settings.url)?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &BTreeMap::new(), Some(&settings.token), settings.insecure)?,
            api: crate::http::join(&base, "api/v1"),
        })
    }

    async fn fetch<T: DeserializeOwned>(&self, url: reqwest::Url) -> anyhow::Result<(T, Meta)> {
        let path = url.path().to_owned();
        let envelope: Envelope<T> = crate::http::get(&self.client, url).await?.json().await?;
        if !envelope.ok {
            let code = envelope.error.map_or_else(|| "unknown".to_owned(), |e| e.code);
            anyhow::bail!("GET {path}: job-scout error {code}");
        }
        let data = envelope.data.ok_or_else(|| anyhow::anyhow!("GET {path}: envelope without data"))?;
        Ok((data, envelope.meta))
    }

    fn url(&self, path: &str) -> reqwest::Url {
        crate::http::join(&self.api, path)
    }

    async fn today(&self, now: DateTime<Utc>) -> anyhow::Result<Values> {
        let (today, _): (Today, _) = self.fetch(self.url("today")).await?;
        Ok(today_values(&today, now))
    }

    async fn discovery(&self) -> anyhow::Result<Values> {
        let mut url = self.url("radar/discovery/summary");
        url.query_pairs_mut().append_pair("hours", "24");
        let (d, _): (Discovery, _) = self.fetch(url).await?;
        let lane = |name: &str| d.lanes.iter().filter(|l| l.lane == name).map(|l| l.c).sum::<f64>();
        Ok(vec![
            number("discovered_24h", d.lanes.iter().map(|l| l.c).sum()),
            number("passed_24h", lane("passed")),
            number("marginal_24h", lane("marginal")),
            number("sources", d.boards.enabled),
            number("sources_errored", d.boards.errored),
        ])
    }

    async fn processes(&self) -> anyhow::Result<Values> {
        let (rows, _): (Vec<Process>, _) = self.fetch(self.url("processes")).await?;
        let mut by_stage: BTreeMap<String, f64> = BTreeMap::new();
        for row in &rows {
            *by_stage.entry(crate::key::sanitize(&row.status)).or_default() += 1.0;
        }
        let mut values = vec![number("processes", rows.len() as f64)];
        values.extend(by_stage.into_iter().map(|(stage, n)| number(format!("process/{stage}/count"), n)));
        Ok(values)
    }

    async fn leads(&self, lane: &str) -> anyhow::Result<Values> {
        let mut url = self.url("leads");
        url.query_pairs_mut().append_pair("lane", lane).append_pair("pageSize", "1");
        let (_, meta): (Vec<serde::de::IgnoredAny>, _) = self.fetch(url).await?;
        let total = meta.total.ok_or_else(|| anyhow::anyhow!("GET /api/v1/leads: no meta.total"))?;
        Ok(vec![number(format!("leads_{lane}"), total)])
    }

    async fn poll_at(&self, now: DateTime<Utc>) -> anyhow::Result<Values> {
        let (today, discovery, processes, needs_reply, call_soon) = tokio::join!(
            self.today(now),
            self.discovery(),
            self.processes(),
            self.leads("needs_reply"),
            self.leads("call_soon")
        );
        let mut partial = crate::partial::Partial::new(&self.name, "endpoints");
        for (endpoint, result) in [
            ("today", today),
            ("radar/discovery/summary", discovery),
            ("processes", processes),
            ("leads needs_reply", needs_reply),
            ("leads call_soon", call_soon),
        ] {
            match result {
                Ok(values) => partial.ok(values),
                Err(e) => partial.fail(endpoint, &e),
            }
        }
        partial.finish()
    }
}

fn today_values(today: &Today, now: DateTime<Utc>) -> Values {
    let counts = &today.counts;
    let mut values = Vec::new();
    for (status, n) in &counts.by_status {
        values.push(number(format!("positions/{}/count", crate::key::sanitize(status)), *n));
    }
    for (status, n) in &counts.last30d {
        values.push(number(format!("last30d/{}/count", crate::key::sanitize(status)), *n));
    }
    values.push(number("last30d", counts.last30d.values().sum()));
    values.push(number("applied_week", counts.applied_this_week));
    values.push(number("untriaged", counts.untriaged));
    let jobs = |statuses: &[&str]| {
        today.queue.iter().filter(|r| statuses.contains(&r.status.as_str())).map(|r| r.c).sum::<f64>()
    };
    values.push(number("queue", jobs(&["queued", "running"])));
    values.push(number("queue_failed", jobs(&["failed"])));
    values.push(number("pending_approvals", today.approvals.len() as f64));
    values.push(number("decisions", today.decisions.len() as f64));
    values.push(number("follow_ups", today.follow_ups.len() as f64));
    values.push(number("upcoming_interviews", today.upcoming.len() as f64));
    if let Some(next) = today.upcoming.iter().filter_map(|r| r.scheduled_at).filter(|at| *at > now).min() {
        values.push(number("next_interview_age", -(next - now).num_seconds() as f64));
    }
    let ops = &today.llm.by_operation;
    values.push(number("llm_requests_today", today.llm.today.values().sum()));
    values.push(number("llm_tokens_24h", ops.iter().map(|o| o.tokens_in + o.tokens_out).sum()));
    values.push(number("llm_failures_24h", ops.iter().map(|o| o.failures).sum()));
    values
}

#[async_trait]
impl Plugin for JobScout {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const TOKEN: &str = "js-test-token-not-real";
    const PRIVATE: [&str; 6] = [
        "Fictional Corp",
        "Staff Platform Engineer",
        "fictional-corp-staff",
        "Jane Recruiter",
        "https://jobs.example.invalid/1",
        "Hiring manager chat",
    ];

    fn envelope(data: serde_json::Value, meta: serde_json::Value) -> ResponseTemplate {
        let mut meta = meta;
        meta["requestId"] = json!("r1");
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "data": data, "meta": meta}))
    }

    fn position(status: &str) -> serde_json::Value {
        json!({
            "id": "p1", "slug": "fictional-corp-staff", "title": "Staff Platform Engineer", "status": status,
            "triageScore": 4.2, "triageVerdict": "pass", "triageOneLiner": "Hiring manager chat",
            "company": {"id": "c1", "slug": "fictional-corp", "name": "Fictional Corp"}
        })
    }

    fn today(now: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
        let soon = (now + chrono::Duration::seconds(3600)).to_rfc3339();
        let later = (now + chrono::Duration::seconds(7200)).to_rfc3339();
        let past = (now - chrono::Duration::seconds(600)).to_rfc3339();
        json!({
            "decisions": [position("triaged"), position("review")],
            "followUps": [position("applied")],
            "changed": [position("screen")],
            "upcoming": [
                {"id": "i0", "positionId": "p1", "leadId": null, "stage": "screen", "scheduledAt": past, "status": "pending",
                 "title": "Staff Platform Engineer", "company": "Fictional Corp", "slug": "fictional-corp-staff"},
                {"id": "i1", "positionId": "p1", "leadId": null, "stage": "hiring_manager", "scheduledAt": soon, "status": "pending",
                 "title": "Staff Platform Engineer", "company": "Fictional Corp", "slug": "fictional-corp-staff"},
                {"id": "i2", "positionId": null, "leadId": "l1", "stage": "screen", "scheduledAt": later, "status": "pending",
                 "title": "Recruiter lead", "company": "Jane Recruiter", "slug": null}
            ],
            "counts": {
                "byStatus": {"triaged": 40, "review": 3, "applied": 5, "interview": 1},
                "untriaged": 2,
                "last30d": {"triaged": 20, "review": 2, "materials": 0, "applied": 3, "screen": 1, "interview": 0,
                            "offer": 0, "rejected": 4, "skip": 6, "archived": 1},
                "appliedThisWeek": 2
            },
            "queue": [
                {"status": "queued", "type": "triage", "c": 4},
                {"status": "running", "type": "evaluate", "c": 1},
                {"status": "done", "type": "triage", "c": 300},
                {"status": "failed", "type": "scan", "c": 2}
            ],
            "llm": {
                "since": "2026-10-24T00:00:00.000Z",
                "byOperation": [
                    {"operation": "triage", "runs": 30, "failures": 1, "tokensIn": 1000, "tokensOut": 200, "avgLatencyMs": 900, "lastAt": null},
                    {"operation": "evaluate", "runs": 2, "failures": 0, "tokensIn": 500, "tokensOut": 300, "avgLatencyMs": 4000, "lastAt": null}
                ],
                "today": {"triage": 12, "evaluate": 1},
                "remaining": [{"operation": "triage", "used": 12, "cap": 100, "remaining": 88}]
            },
            "approvals": [
                {"id": "a1", "kind": "status", "status": "pending", "title": "Staff Platform Engineer", "body": "Fictional Corp",
                 "position": {"slug": "fictional-corp-staff", "title": "Staff Platform Engineer", "status": "triaged", "company": "Fictional Corp"}}
            ],
            "activity": [{"id": "t1", "kind": "note", "title": "Hiring manager chat", "company": "Fictional Corp"}]
        })
    }

    fn process(status: &str) -> serde_json::Value {
        json!({
            "id": "p1", "slug": "fictional-corp-staff", "title": "Staff Platform Engineer", "status": status,
            "priority": "high", "nextAction": "https://jobs.example.invalid/1", "notes": null,
            "company": {"id": "c1", "slug": "fictional-corp", "name": "Fictional Corp"},
            "roundCount": 2, "pendingCount": 1, "completedCount": 1, "lastRound": null, "nextRound": null, "rounds": []
        })
    }

    async fn mount(server: &MockServer, route: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/{route}")))
            .and(header("authorization", format!("Bearer {TOKEN}")))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn full_server(now: chrono::DateTime<chrono::Utc>) -> MockServer {
        let server = MockServer::start().await;
        mount(&server, "today", envelope(today(now), json!({}))).await;
        let summary = json!({
            "since": "2026-10-24T00:00:00.000Z",
            "lanes": [{"lane": "passed", "c": 7, "positions": 6}, {"lane": "marginal", "c": 3, "positions": 3},
                      {"lane": "filtered", "c": 90, "positions": 80}],
            "reasons": [{"reason": "geo", "c": 50}],
            "boards": {"total": 40, "enabled": 35, "scanned24h": 30, "errored": 2}
        });
        Mock::given(method("GET"))
            .and(path("/api/v1/radar/discovery/summary"))
            .and(query_param("hours", "24"))
            .and(header("authorization", format!("Bearer {TOKEN}")))
            .respond_with(envelope(summary, json!({})))
            .mount(&server)
            .await;
        let processes = json!([process("applied"), process("applied"), process("interview")]);
        mount(&server, "processes", envelope(processes, json!({"total": 3}))).await;
        for (lane, total) in [("needs_reply", 4), ("call_soon", 1)] {
            Mock::given(method("GET"))
                .and(path("/api/v1/leads"))
                .and(query_param("lane", lane))
                .and(query_param("pageSize", "1"))
                .and(header("authorization", format!("Bearer {TOKEN}")))
                .respond_with(envelope(
                    json!([{"id": "l1", "recruiterName": "Jane Recruiter", "pitchedRole": "Staff Platform Engineer"}]),
                    json!({"total": total, "page": 1, "pageSize": 1, "nextCursor": null}),
                ))
                .mount(&server)
                .await;
        }
        server
    }

    fn plugin(server: &MockServer, token: &str) -> JobScout {
        let yaml = format!("url: {}\ntoken: {token}", server.uri());
        JobScout::new("jobs", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    fn private_leak(got: &BTreeMap<String, Value>) -> Option<String> {
        got.iter().find_map(|(k, v)| {
            let text = format!("{k} {}", v.as_text().unwrap_or_default());
            PRIVATE.iter().find(|p| text.contains(*p)).map(|p| format!("{k}: {p}"))
        })
    }

    #[tokio::test]
    async fn reads_every_aggregate() {
        let now = chrono::Utc::now();
        let server = full_server(now).await;
        let got: BTreeMap<_, _> = plugin(&server, TOKEN).poll_at(now).await.unwrap().into_iter().collect();
        let n = |k: &str| got.get(k).and_then(Value::as_f64).unwrap_or_else(|| panic!("{k} missing"));
        assert_eq!(n("positions/triaged/count"), 40.0);
        assert_eq!(n("positions/interview/count"), 1.0);
        assert_eq!(n("last30d"), 37.0);
        assert_eq!(n("last30d/rejected/count"), 4.0);
        assert_eq!(n("last30d/materials/count"), 0.0);
        assert_eq!(n("applied_week"), 2.0);
        assert_eq!(n("untriaged"), 2.0);
        assert_eq!(n("queue"), 5.0);
        assert_eq!(n("queue_failed"), 2.0);
        assert_eq!(n("pending_approvals"), 1.0);
        assert_eq!(n("upcoming_interviews"), 3.0);
        assert_eq!(n("next_interview_age"), -3600.0);
        assert_eq!(n("decisions"), 2.0);
        assert_eq!(n("follow_ups"), 1.0);
        assert_eq!(n("llm_requests_today"), 13.0);
        assert_eq!(n("llm_tokens_24h"), 2000.0);
        assert_eq!(n("llm_failures_24h"), 1.0);
        assert_eq!(n("discovered_24h"), 100.0);
        assert_eq!(n("passed_24h"), 7.0);
        assert_eq!(n("marginal_24h"), 3.0);
        assert_eq!(n("sources"), 35.0);
        assert_eq!(n("sources_errored"), 2.0);
        assert_eq!(n("processes"), 3.0);
        assert_eq!(n("process/applied/count"), 2.0);
        assert_eq!(n("process/interview/count"), 1.0);
        assert_eq!(n("leads_needs_reply"), 4.0);
        assert_eq!(n("leads_call_soon"), 1.0);
        assert_eq!(n("_endpoints_ok"), 5.0);
        assert_eq!(n("_endpoints_failed"), 0.0);
        assert_eq!(private_leak(&got), None);
    }

    #[tokio::test]
    async fn no_upcoming_round_means_no_age() {
        let now = chrono::Utc::now();
        let server = MockServer::start().await;
        let mut body = today(now);
        body["upcoming"] = json!([]);
        mount(&server, "today", envelope(body, json!({}))).await;
        let got: BTreeMap<_, _> = plugin(&server, TOKEN).poll_at(now).await.unwrap().into_iter().collect();
        assert_eq!(got["upcoming_interviews"], Value::Number(0.0));
        assert!(!got.contains_key("next_interview_age"));
    }

    #[tokio::test]
    async fn a_failing_endpoint_leaves_the_others() {
        let now = chrono::Utc::now();
        let server = MockServer::start().await;
        mount(&server, "processes", envelope(json!([process("offer")]), json!({"total": 1}))).await;
        let got: BTreeMap<_, _> = plugin(&server, TOKEN).poll_at(now).await.unwrap().into_iter().collect();
        assert_eq!(got["process/offer/count"], Value::Number(1.0));
        assert_eq!(got["_endpoints_ok"], Value::Number(1.0));
        assert_eq!(got["_endpoints_failed"], Value::Number(4.0));
        assert!(!got.contains_key("queue"));
    }

    #[tokio::test]
    async fn an_error_envelope_fails_that_endpoint_by_code() {
        let server = MockServer::start().await;
        let refused = ResponseTemplate::new(200).set_body_json(json!({
            "ok": false, "error": {"code": "FORBIDDEN", "message": "Fictional Corp", "details": {}}, "meta": {"requestId": "r"}
        }));
        mount(&server, "processes", refused).await;
        let err = format!("{:#}", plugin(&server, TOKEN).poll().await.unwrap_err());
        assert!(err.contains("FORBIDDEN") && !err.contains("Fictional Corp"), "{err}");
    }

    #[tokio::test]
    async fn a_wrong_token_fails_without_echoing_it() {
        let now = chrono::Utc::now();
        let server = full_server(now).await;
        let err = format!("{:#}", plugin(&server, "wrong-token-value").poll().await.unwrap_err());
        assert!(err.contains("404") && !err.contains("wrong-token-value"), "{err}");
    }

    #[test]
    fn defaults() {
        let s: Settings = serde_yaml::from_str("{url: 'https://jobs.example.invalid', token: t}").unwrap();
        assert!(!s.insecure && s.timeout_s.is_none());
        assert_eq!(JobScout::new("j", s).unwrap().interval(), Duration::from_secs(300));
        let zero: Settings =
            serde_yaml::from_str("{url: 'https://jobs.example.invalid', token: t, timeout_s: 0}").unwrap();
        assert!(JobScout::new("j", zero).is_err());
    }
}
