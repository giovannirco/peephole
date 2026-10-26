//! The *arr family plus Seerr and Jellyfin: versions, health, queues, library
//! counts, recent imports and requests. Counts and sizes only: never a title,
//! path, release name, user or indexer name.
//!
//! Endpoints per `kind` (`X-Api-Key: <key>` except Jellyfin):
//! - `radarr` / `sonarr` (`/api/v3`) and `lidarr` (`/api/v1`): `GET system/status`, `GET health`,
//!   `GET queue?page=1&pageSize=100`, `GET movie` / `series` / `artist`,
//!   `GET calendar?start=&end=` (radarr, sonarr), `GET history?page=1&pageSize=50&sortKey=date&sortDirection=descending`.
//! - `prowlarr` (`/api/v1`): `GET system/status`, `GET health`, `GET indexer`, `GET indexerstats`
//!   (no dates: all time).
//! - `seerr` (`/api/v1`): `GET status`, `GET request/count`.
//! - `jellyfin` (`Authorization: MediaBrowser Token="<key>"`): `GET /System/Info/Public`,
//!   `GET /Sessions?activeWithinSeconds=300`, `GET /Items/Counts`.
//!
//! Verified against the OpenAPI documents in the upstream repositories
//! (`Radarr.Api.V3/openapi.json`, `Sonarr.Api.V3/openapi.json`, `Lidarr.Api.V1/openapi.json`,
//! `Prowlarr.Api.V1/openapi.json`, Prowlarr's `IndexerStatsController.cs`), Seerr's
//! `seerr-api.yml`, Jellyfin's `jellyfin-openapi-stable.json` (12.3.0) and its
//! `AuthorizationContext.cs` (the `MediaBrowser` scheme with a `Token` field).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use peephole_core::{Plugin, Value};
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};

/// No `Debug`: the apps hold API keys.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub apps: Vec<AppSettings>,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// No `Debug`: holds an API key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSettings {
    /// Key segment for this app's values.
    pub name: String,
    pub kind: AppKind,
    pub url: String,
    /// Sent as a header only; never logged.
    pub api_key: String,
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppKind {
    Radarr,
    Sonarr,
    Lidarr,
    Prowlarr,
    Seerr,
    Jellyfin,
}

impl AppKind {
    fn api(self) -> &'static str {
        match self {
            AppKind::Radarr | AppKind::Sonarr => "api/v3",
            AppKind::Lidarr | AppKind::Prowlarr | AppKind::Seerr => "api/v1",
            AppKind::Jellyfin => "",
        }
    }
}

fn default_interval() -> u64 {
    60
}

#[derive(Deserialize)]
struct SystemStatus {
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct JellyfinInfo {
    version: String,
}

#[derive(Deserialize)]
struct Health {
    source: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Queue {
    total_records: f64,
    records: Vec<QueueItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueItem {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    tracked_download_status: Option<String>,
    #[serde(default)]
    sizeleft: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Movie {
    monitored: bool,
    has_file: bool,
    #[serde(default)]
    size_on_disk: f64,
}

#[derive(Deserialize)]
struct Series {
    monitored: bool,
    #[serde(default)]
    statistics: Option<SeriesStatistics>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesStatistics {
    episode_count: f64,
    episode_file_count: f64,
    #[serde(default)]
    size_on_disk: f64,
}

#[derive(Deserialize)]
struct Artist {
    #[serde(default)]
    statistics: Option<ArtistStatistics>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtistStatistics {
    album_count: f64,
    track_count: f64,
    track_file_count: f64,
    #[serde(default)]
    size_on_disk: f64,
}

#[derive(Deserialize)]
struct History {
    records: Vec<HistoryRecord>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryRecord {
    event_type: String,
    date: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Indexer {
    enable: bool,
}

#[derive(Deserialize)]
struct IndexerStats {
    indexers: Vec<IndexerStat>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexerStat {
    number_of_queries: f64,
    number_of_grabs: f64,
    number_of_failed_queries: f64,
}

#[derive(Deserialize)]
struct RequestCount {
    total: f64,
    movie: f64,
    tv: f64,
    pending: f64,
    approved: f64,
    processing: f64,
    available: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Session {
    #[serde(default)]
    now_playing_item: Option<IgnoredAny>,
    #[serde(default)]
    transcoding_info: Option<IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemCounts {
    movie_count: f64,
    series_count: f64,
    episode_count: f64,
    song_count: f64,
}

type Values = Vec<(String, Value)>;
type Parts = Vec<(&'static str, anyhow::Result<Values>)>;

const STATUS_PART: &str = "status";
const HISTORY_PAGE: &str = "50";

struct App {
    key: String,
    kind: AppKind,
    api: reqwest::Url,
    client: reqwest::Client,
}

impl App {
    fn new(settings: AppSettings, timeout: Duration) -> anyhow::Result<Self> {
        let (header, value) = match settings.kind {
            AppKind::Jellyfin => ("Authorization", format!("MediaBrowser Token=\"{}\"", settings.api_key)),
            _ => ("X-Api-Key", settings.api_key),
        };
        let headers = BTreeMap::from([(header.to_owned(), value)]);
        let base = crate::http::base_url(&settings.url)?;
        Ok(Self {
            key: crate::key::sanitize(&settings.name),
            kind: settings.kind,
            api: crate::http::join(&base, settings.kind.api()),
            client: crate::http::client(timeout, &headers, None, settings.insecure)?,
        })
    }

    fn k(&self, metric: &str) -> String {
        format!("{}/{metric}", self.key)
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> reqwest::Url {
        let mut url = crate::http::join(&self.api, path);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    async fn json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<T> {
        let url = self.url(path, query);
        let at = url.path().to_owned();
        crate::http::get(&self.client, url).await?.json().await.with_context(|| format!("GET {at}: unexpected body"))
    }

    fn number(&self, metric: &str, n: f64) -> (String, Value) {
        (self.k(metric), Value::Number(n))
    }

    async fn status(&self) -> anyhow::Result<Values> {
        let version = match self.kind {
            AppKind::Jellyfin => self.json::<JellyfinInfo>("System/Info/Public", &[]).await?.version,
            AppKind::Seerr => self.json::<SystemStatus>("status", &[]).await?.version,
            _ => self.json::<SystemStatus>("system/status", &[]).await?.version,
        };
        Ok(vec![(self.k("version"), Value::Text(version))])
    }

    async fn health(&self) -> anyhow::Result<Values> {
        let checks: Vec<Health> = self.json("health", &[]).await?;
        let first = |kind: &str| checks.iter().find(|c| c.kind == kind).map(|c| c.source.clone());
        let worst = first("error").or_else(|| first("warning")).unwrap_or_default();
        Ok(vec![
            self.number("health_issues", checks.len() as f64),
            self.number("health_errors", checks.iter().filter(|c| c.kind == "error").count() as f64),
            (self.k("health_worst"), Value::Text(worst)),
        ])
    }

    async fn queue(&self) -> anyhow::Result<Values> {
        let q: Queue = self.json("queue", &[("page", "1"), ("pageSize", "100")]).await?;
        let count = |f: &dyn Fn(&QueueItem) -> bool| q.records.iter().filter(|r| f(r)).count() as f64;
        Ok(vec![
            self.number("queue", q.total_records),
            self.number("queue_downloading", count(&|r| r.status.as_deref() == Some("downloading"))),
            self.number("queue_warnings", count(&|r| r.tracked_download_status.as_deref() == Some("warning"))),
            self.number("queue_bytes_left", q.records.iter().map(|r| r.sizeleft).sum()),
        ])
    }

    async fn library(&self) -> anyhow::Result<Values> {
        match self.kind {
            AppKind::Radarr => {
                let movies: Vec<Movie> = self.json("movie", &[]).await?;
                Ok(vec![
                    self.number("items", movies.len() as f64),
                    self.number("missing", movies.iter().filter(|m| m.monitored && !m.has_file).count() as f64),
                    self.number("size_bytes", movies.iter().map(|m| m.size_on_disk).sum()),
                ])
            }
            AppKind::Sonarr => {
                let series: Vec<Series> = self.json("series", &[]).await?;
                let stats = || series.iter().filter_map(|s| s.statistics.as_ref());
                let missing = series
                    .iter()
                    .filter(|s| s.monitored)
                    .filter_map(|s| s.statistics.as_ref())
                    .map(|st| (st.episode_count - st.episode_file_count).max(0.0))
                    .sum();
                Ok(vec![
                    self.number("items", series.len() as f64),
                    self.number("missing", missing),
                    self.number("episodes", stats().map(|st| st.episode_count).sum()),
                    self.number("size_bytes", stats().map(|st| st.size_on_disk).sum()),
                ])
            }
            _ => {
                let artists: Vec<Artist> = self.json("artist", &[]).await?;
                let stats = || artists.iter().filter_map(|a| a.statistics.as_ref());
                Ok(vec![
                    self.number("items", artists.len() as f64),
                    self.number("albums", stats().map(|st| st.album_count).sum()),
                    self.number("missing", stats().map(|st| (st.track_count - st.track_file_count).max(0.0)).sum()),
                    self.number("size_bytes", stats().map(|st| st.size_on_disk).sum()),
                ])
            }
        }
    }

    async fn calendar(&self, now: DateTime<Utc>) -> anyhow::Result<Values> {
        let start = now.date_naive().and_hms_opt(0, 0, 0).expect("midnight exists").and_utc();
        let end = start + chrono::Duration::days(7);
        let format = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let (start, end) = (format(start), format(end));
        let items: Vec<IgnoredAny> = self.json("calendar", &[("start", &start), ("end", &end)]).await?;
        Ok(vec![self.number("upcoming_7d", items.len() as f64)])
    }

    async fn history(&self, now: DateTime<Utc>) -> anyhow::Result<Values> {
        let query = [("page", "1"), ("pageSize", HISTORY_PAGE), ("sortKey", "date"), ("sortDirection", "descending")];
        let h: History = self.json("history", &query).await?;
        let imported = if self.kind == AppKind::Lidarr { "trackFileImported" } else { "downloadFolderImported" };
        let since = now - chrono::Duration::hours(24);
        let recent = |event: &str| h.records.iter().filter(|r| r.event_type == event && r.date >= since).count();
        Ok(vec![
            self.number("imported_24h", recent(imported) as f64),
            self.number("grabbed_24h", recent("grabbed") as f64),
        ])
    }

    async fn indexers(&self) -> anyhow::Result<Values> {
        let indexers: Vec<Indexer> = self.json("indexer", &[]).await?;
        Ok(vec![
            self.number("indexers", indexers.len() as f64),
            self.number("indexers_enabled", indexers.iter().filter(|i| i.enable).count() as f64),
        ])
    }

    async fn indexer_stats(&self) -> anyhow::Result<Values> {
        let stats: IndexerStats = self.json("indexerstats", &[]).await?;
        let total = |f: fn(&IndexerStat) -> f64| stats.indexers.iter().map(f).sum();
        Ok(vec![
            self.number("queries_total", total(|s| s.number_of_queries)),
            self.number("grabs_total", total(|s| s.number_of_grabs)),
            self.number("failed_queries", total(|s| s.number_of_failed_queries)),
        ])
    }

    async fn requests(&self) -> anyhow::Result<Values> {
        let c: RequestCount = self.json("request/count", &[]).await?;
        Ok(vec![
            self.number("requests_total", c.total),
            self.number("requests_pending", c.pending),
            self.number("requests_approved", c.approved),
            self.number("requests_available", c.available),
            self.number("requests_processing", c.processing),
            self.number("requests_movie", c.movie),
            self.number("requests_tv", c.tv),
        ])
    }

    async fn sessions(&self) -> anyhow::Result<Values> {
        let sessions: Vec<Session> = self.json("Sessions", &[("activeWithinSeconds", "300")]).await?;
        Ok(vec![
            self.number("sessions", sessions.len() as f64),
            self.number("playing", sessions.iter().filter(|s| s.now_playing_item.is_some()).count() as f64),
            self.number("transcoding", sessions.iter().filter(|s| s.transcoding_info.is_some()).count() as f64),
        ])
    }

    async fn item_counts(&self) -> anyhow::Result<Values> {
        let c: ItemCounts = self.json("Items/Counts", &[]).await?;
        Ok(vec![
            self.number("movies", c.movie_count),
            self.number("series", c.series_count),
            self.number("episodes", c.episode_count),
            self.number("songs", c.song_count),
        ])
    }

    async fn poll(&self, now: DateTime<Utc>) -> Parts {
        match self.kind {
            AppKind::Radarr | AppKind::Sonarr | AppKind::Lidarr => {
                let calendar =
                    async { if self.kind == AppKind::Lidarr { None } else { Some(self.calendar(now).await) } };
                let (status, health, queue, library, history, calendar) = tokio::join!(
                    self.status(),
                    self.health(),
                    self.queue(),
                    self.library(),
                    self.history(now),
                    calendar
                );
                let mut parts = vec![
                    (STATUS_PART, status),
                    ("health", health),
                    ("queue", queue),
                    ("library", library),
                    ("history", history),
                ];
                parts.extend(calendar.map(|c| ("calendar", c)));
                parts
            }
            AppKind::Prowlarr => {
                let (status, health, indexers, stats) =
                    tokio::join!(self.status(), self.health(), self.indexers(), self.indexer_stats());
                vec![(STATUS_PART, status), ("health", health), ("indexer", indexers), ("indexerstats", stats)]
            }
            AppKind::Seerr => {
                let (status, requests) = tokio::join!(self.status(), self.requests());
                vec![(STATUS_PART, status), ("request/count", requests)]
            }
            AppKind::Jellyfin => {
                let (status, sessions, counts) = tokio::join!(self.status(), self.sessions(), self.item_counts());
                vec![(STATUS_PART, status), ("Sessions", sessions), ("Items/Counts", counts)]
            }
        }
    }
}

pub struct Servarr {
    name: String,
    interval: Duration,
    apps: Vec<App>,
}

impl Servarr {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(t) => crate::period::seconds(t, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        anyhow::ensure!(!settings.apps.is_empty(), "apps must list at least one app");
        let apps = settings
            .apps
            .into_iter()
            .map(|a| {
                let label = a.name.clone();
                App::new(a, timeout).with_context(|| format!("app {label:?}"))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { name: name.to_owned(), interval, apps })
    }

    async fn poll_at(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<(String, Value)>> {
        let polled = futures::future::join_all(self.apps.iter().map(|a| a.poll(now))).await;
        let mut partial = crate::partial::Partial::new(&self.name, "endpoints");
        let mut up = Vec::new();
        for (app, parts) in self.apps.iter().zip(polled) {
            for (part, result) in parts {
                if part == STATUS_PART {
                    up.push((app.k("up"), result.is_ok()));
                }
                match result {
                    Ok(values) => partial.ok(values),
                    Err(e) => partial.fail(&format!("{} {part}", app.key), &e),
                }
            }
        }
        let mut values = partial.finish()?;
        let totals = ["queue", "missing", "health_issues", "upcoming_7d", "imported_24h"]
            .map(|metric| (metric, sum(&values, &self.apps, metric)));
        values.push(("apps_total".into(), Value::Number(self.apps.len() as f64)));
        values.push(("apps_up".into(), Value::Number(up.iter().filter(|(_, ok)| *ok).count() as f64)));
        for (key, ok) in up {
            values.push((key, Value::Number(if ok { 1.0 } else { 0.0 })));
        }
        for (metric, total) in totals {
            values.push((metric.to_owned(), Value::Number(total)));
        }
        Ok(values)
    }
}

fn sum(values: &Values, apps: &[App], metric: &str) -> f64 {
    let keys: Vec<String> = apps.iter().map(|a| a.k(metric)).collect();
    values.iter().filter(|(k, _)| keys.contains(k)).filter_map(|(_, v)| v.as_f64()).sum()
}

#[async_trait]
impl Plugin for Servarr {
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

    use chrono::TimeZone;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const KEY: &str = "arr-key-not-real";
    const PRIVATE: [&str; 5] =
        ["The Secret Film", "Private Show", "Hidden Band", "Secret.Film.2026.1080p", "/data/media/private"];

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 15, 30, 0).unwrap()
    }

    fn ago(hours: i64) -> String {
        (now() - chrono::Duration::hours(hours)).to_rfc3339()
    }

    fn ok(body: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(body)
    }

    async fn mount(server: &MockServer, route: &str, auth: (&str, &str), response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header(auth.0, auth.1))
            .respond_with(response)
            .mount(server)
            .await;
    }

    const ARR_KEY: (&str, &str) = ("x-api-key", KEY);

    fn health() -> serde_json::Value {
        json!([
            {"source": "IndexerSearchCheck", "type": "warning", "message": "No indexers available", "wikiUrl": "https://wiki.example.invalid"},
            {"source": "DownloadClientCheck", "type": "error", "message": "Unable to reach /data/media/private", "wikiUrl": ""},
            {"source": "UpdateCheck", "type": "notice", "message": "Update", "wikiUrl": ""}
        ])
    }

    fn queue() -> serde_json::Value {
        json!({"page": 1, "pageSize": 100, "sortKey": "timeleft", "sortDirection": "ascending", "totalRecords": 3, "records": [
            {"id": 1, "title": "Secret.Film.2026.1080p", "status": "downloading", "trackedDownloadStatus": "ok", "size": 1000, "sizeleft": 400, "outputPath": "/data/media/private"},
            {"id": 2, "title": "Secret.Film.2026.1080p", "status": "completed", "trackedDownloadStatus": "warning", "size": 500, "sizeleft": 0},
            {"id": 3, "title": "Secret.Film.2026.1080p", "status": "queued", "size": 300, "sizeleft": 300}
        ]})
    }

    fn history(imported: &str) -> serde_json::Value {
        json!({"page": 1, "pageSize": 50, "sortKey": "date", "sortDirection": "descending", "totalRecords": 4, "records": [
            {"id": 4, "eventType": imported, "date": ago(1), "sourceTitle": "Secret.Film.2026.1080p"},
            {"id": 3, "eventType": "grabbed", "date": ago(2), "sourceTitle": "Secret.Film.2026.1080p"},
            {"id": 2, "eventType": "grabbed", "date": ago(23), "sourceTitle": "Secret.Film.2026.1080p"},
            {"id": 1, "eventType": imported, "date": ago(30), "sourceTitle": "Secret.Film.2026.1080p"}
        ]})
    }

    async fn arr(
        version: &str,
        v: &str,
        library: (&str, serde_json::Value),
        calendar: Option<serde_json::Value>,
        imported: &str,
    ) -> MockServer {
        let server = MockServer::start().await;
        let api = |p: &str| format!("/api/{v}/{p}");
        mount(
            &server,
            &api("system/status"),
            ARR_KEY,
            ok(json!({"appName": "Radarr", "version": version, "startupPath": "/app"})),
        )
        .await;
        mount(&server, &api("health"), ARR_KEY, ok(health())).await;
        Mock::given(method("GET"))
            .and(path(api("queue")))
            .and(query_param("page", "1"))
            .and(query_param("pageSize", "100"))
            .and(header(ARR_KEY.0, ARR_KEY.1))
            .respond_with(ok(queue()))
            .mount(&server)
            .await;
        mount(&server, &api(library.0), ARR_KEY, ok(library.1)).await;
        if let Some(body) = calendar {
            Mock::given(method("GET"))
                .and(path(api("calendar")))
                .and(query_param("start", "2026-10-25T00:00:00Z"))
                .and(query_param("end", "2026-11-01T00:00:00Z"))
                .and(header(ARR_KEY.0, ARR_KEY.1))
                .respond_with(ok(body))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path(api("history")))
            .and(query_param("pageSize", "50"))
            .and(query_param("sortKey", "date"))
            .and(query_param("sortDirection", "descending"))
            .and(header(ARR_KEY.0, ARR_KEY.1))
            .respond_with(ok(history(imported)))
            .mount(&server)
            .await;
        server
    }

    async fn radarr() -> MockServer {
        let movies = json!([
            {"id": 1, "title": "The Secret Film", "path": "/data/media/private", "monitored": true, "hasFile": true, "sizeOnDisk": 4000},
            {"id": 2, "title": "The Secret Film", "monitored": true, "hasFile": false, "sizeOnDisk": 0},
            {"id": 3, "title": "The Secret Film", "monitored": false, "hasFile": false}
        ]);
        let calendar = json!([{"id": 2, "title": "The Secret Film"}]);
        arr("5.20.1", "v3", ("movie", movies), Some(calendar), "downloadFolderImported").await
    }

    async fn sonarr() -> MockServer {
        let series = json!([
            {"id": 1, "title": "Private Show", "monitored": true,
             "statistics": {"seasonCount": 2, "episodeFileCount": 18, "episodeCount": 20, "totalEpisodeCount": 22, "sizeOnDisk": 9000, "percentOfEpisodes": 90.0}},
            {"id": 2, "title": "Private Show", "monitored": false,
             "statistics": {"seasonCount": 1, "episodeFileCount": 0, "episodeCount": 10, "totalEpisodeCount": 10, "sizeOnDisk": 0, "percentOfEpisodes": 0.0}}
        ]);
        let calendar = json!([{"id": 7, "title": "Private Show"}, {"id": 8, "title": "Private Show"}]);
        arr("4.0.15", "v3", ("series", series), Some(calendar), "downloadFolderImported").await
    }

    async fn lidarr() -> MockServer {
        let artists = json!([
            {"id": 1, "artistName": "Hidden Band", "monitored": true,
             "statistics": {"albumCount": 3, "trackFileCount": 25, "trackCount": 30, "totalTrackCount": 30, "sizeOnDisk": 700, "percentOfTracks": 83.3}},
            {"id": 2, "artistName": "Hidden Band", "monitored": true,
             "statistics": {"albumCount": 1, "trackFileCount": 10, "trackCount": 10, "totalTrackCount": 12, "sizeOnDisk": 300, "percentOfTracks": 100.0}}
        ]);
        arr("2.13.3", "v1", ("artist", artists), None, "trackFileImported").await
    }

    async fn prowlarr() -> MockServer {
        let server = MockServer::start().await;
        mount(&server, "/api/v1/system/status", ARR_KEY, ok(json!({"appName": "Prowlarr", "version": "2.1.5"}))).await;
        mount(&server, "/api/v1/health", ARR_KEY, ok(json!([]))).await;
        let indexers =
            json!([{"id": 1, "name": "Tracker A", "enable": true}, {"id": 2, "name": "Tracker B", "enable": false}]);
        mount(&server, "/api/v1/indexer", ARR_KEY, ok(indexers)).await;
        let stats = json!({"id": 1, "indexers": [
            {"indexerId": 1, "indexerName": "Tracker A", "averageResponseTime": 300, "numberOfQueries": 120, "numberOfGrabs": 10, "numberOfFailedQueries": 3},
            {"indexerId": 2, "indexerName": "Tracker B", "averageResponseTime": 900, "numberOfQueries": 30, "numberOfGrabs": 2, "numberOfFailedQueries": 1}
        ], "userAgents": [], "hosts": []});
        mount(&server, "/api/v1/indexerstats", ARR_KEY, ok(stats)).await;
        server
    }

    async fn seerr() -> MockServer {
        let server = MockServer::start().await;
        mount(&server, "/api/v1/status", ARR_KEY, ok(json!({"version": "3.0.1", "commitTag": "v3.0.1"}))).await;
        let counts = json!({"total": 40, "movie": 25, "tv": 15, "pending": 2, "approved": 30, "declined": 1,
                            "processing": 3, "available": 28, "completed": 28});
        mount(&server, "/api/v1/request/count", ARR_KEY, ok(counts)).await;
        server
    }

    async fn jellyfin() -> MockServer {
        let server = MockServer::start().await;
        let auth = ("authorization", "MediaBrowser Token=\"arr-key-not-real\"");
        mount(&server, "/System/Info/Public", auth, ok(json!({"ServerName": "Home", "Version": "10.11.2", "Id": "x"})))
            .await;
        let sessions = json!([
            {"Id": "s1", "UserName": "someone", "DeviceName": "TV", "NowPlayingItem": {"Name": "The Secret Film"}, "TranscodingInfo": {"VideoCodec": "h264"}},
            {"Id": "s2", "UserName": "other", "NowPlayingItem": {"Name": "Private Show"}},
            {"Id": "s3", "UserName": "idle", "NowPlayingItem": null}
        ]);
        Mock::given(method("GET"))
            .and(path("/Sessions"))
            .and(query_param("activeWithinSeconds", "300"))
            .and(header(auth.0, auth.1))
            .respond_with(ok(sessions))
            .mount(&server)
            .await;
        let counts = json!({"MovieCount": 410, "SeriesCount": 52, "EpisodeCount": 2300, "SongCount": 8000, "AlbumCount": 600, "ItemCount": 0});
        mount(&server, "/Items/Counts", auth, ok(counts)).await;
        server
    }

    fn plugin(apps: &[(&str, &str, &MockServer)], key: &str) -> Servarr {
        let list: Vec<String> = apps
            .iter()
            .map(|(name, kind, s)| format!("  - {{name: {name}, kind: {kind}, url: '{}', api_key: '{key}'}}", s.uri()))
            .collect();
        Servarr::new("media", serde_yaml::from_str(&format!("apps:\n{}", list.join("\n"))).unwrap()).unwrap()
    }

    fn leak(got: &BTreeMap<String, Value>) -> Option<String> {
        got.iter().find_map(|(k, v)| {
            let text = format!("{k} {}", v.as_text().unwrap_or_default());
            PRIVATE.iter().find(|p| text.contains(*p)).map(|p| format!("{k}: {p}"))
        })
    }

    #[tokio::test]
    async fn reads_every_kind() {
        let servers =
            [radarr().await, sonarr().await, lidarr().await, prowlarr().await, seerr().await, jellyfin().await];
        let apps = [
            ("movies", "radarr", &servers[0]),
            ("tv", "sonarr", &servers[1]),
            ("music", "lidarr", &servers[2]),
            ("indexers", "prowlarr", &servers[3]),
            ("requests", "seerr", &servers[4]),
            ("jellyfin", "jellyfin", &servers[5]),
        ];
        let got: BTreeMap<_, _> = plugin(&apps, KEY).poll_at(now()).await.unwrap().into_iter().collect();
        let n = |k: &str| got.get(k).and_then(Value::as_f64).unwrap_or_else(|| panic!("{k} missing"));
        let t = |k: &str| got.get(k).and_then(Value::as_text).unwrap_or_else(|| panic!("{k} missing")).to_owned();

        assert_eq!(t("movies/version"), "5.20.1");
        assert_eq!(n("movies/up"), 1.0);
        assert_eq!(n("movies/health_issues"), 3.0);
        assert_eq!(n("movies/health_errors"), 1.0);
        assert_eq!(t("movies/health_worst"), "DownloadClientCheck");
        assert_eq!(n("movies/queue"), 3.0);
        assert_eq!(n("movies/queue_downloading"), 1.0);
        assert_eq!(n("movies/queue_warnings"), 1.0);
        assert_eq!(n("movies/queue_bytes_left"), 700.0);
        assert_eq!(n("movies/items"), 3.0);
        assert_eq!(n("movies/missing"), 1.0);
        assert_eq!(n("movies/size_bytes"), 4000.0);
        assert_eq!(n("movies/upcoming_7d"), 1.0);
        assert_eq!(n("movies/imported_24h"), 1.0);
        assert_eq!(n("movies/grabbed_24h"), 2.0);

        assert_eq!(t("tv/version"), "4.0.15");
        assert_eq!(n("tv/items"), 2.0);
        assert_eq!(n("tv/missing"), 2.0);
        assert_eq!(n("tv/episodes"), 30.0);
        assert_eq!(n("tv/size_bytes"), 9000.0);
        assert_eq!(n("tv/upcoming_7d"), 2.0);
        assert_eq!(n("tv/imported_24h"), 1.0);

        assert_eq!(t("music/version"), "2.13.3");
        assert_eq!(n("music/items"), 2.0);
        assert_eq!(n("music/albums"), 4.0);
        assert_eq!(n("music/missing"), 5.0);
        assert_eq!(n("music/size_bytes"), 1000.0);
        assert_eq!(n("music/imported_24h"), 1.0);
        assert!(!got.contains_key("music/upcoming_7d"));

        assert_eq!(t("indexers/version"), "2.1.5");
        assert_eq!(n("indexers/health_issues"), 0.0);
        assert_eq!(t("indexers/health_worst"), "");
        assert_eq!(n("indexers/indexers"), 2.0);
        assert_eq!(n("indexers/indexers_enabled"), 1.0);
        assert_eq!(n("indexers/queries_total"), 150.0);
        assert_eq!(n("indexers/grabs_total"), 12.0);
        assert_eq!(n("indexers/failed_queries"), 4.0);

        assert_eq!(t("requests/version"), "3.0.1");
        assert_eq!(n("requests/requests_total"), 40.0);
        assert_eq!(n("requests/requests_pending"), 2.0);
        assert_eq!(n("requests/requests_approved"), 30.0);
        assert_eq!(n("requests/requests_available"), 28.0);
        assert_eq!(n("requests/requests_processing"), 3.0);
        assert_eq!(n("requests/requests_movie"), 25.0);
        assert_eq!(n("requests/requests_tv"), 15.0);

        assert_eq!(t("jellyfin/version"), "10.11.2");
        assert_eq!(n("jellyfin/sessions"), 3.0);
        assert_eq!(n("jellyfin/playing"), 2.0);
        assert_eq!(n("jellyfin/transcoding"), 1.0);
        assert_eq!(n("jellyfin/movies"), 410.0);
        assert_eq!(n("jellyfin/series"), 52.0);
        assert_eq!(n("jellyfin/episodes"), 2300.0);
        assert_eq!(n("jellyfin/songs"), 8000.0);

        assert_eq!(n("apps_total"), 6.0);
        assert_eq!(n("apps_up"), 6.0);
        assert_eq!(n("queue"), 9.0);
        assert_eq!(n("missing"), 8.0);
        assert_eq!(n("health_issues"), 9.0);
        assert_eq!(n("upcoming_7d"), 3.0);
        assert_eq!(n("imported_24h"), 3.0);
        assert_eq!(n("_endpoints_failed"), 0.0);
        assert_eq!(leak(&got), None);
    }

    #[tokio::test]
    async fn a_down_app_leaves_the_others() {
        let up = seerr().await;
        let down = MockServer::start().await;
        let got: BTreeMap<_, _> = plugin(&[("requests", "seerr", &up), ("movies", "radarr", &down)], KEY)
            .poll_at(now())
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(got["requests/up"], Value::Number(1.0));
        assert_eq!(got["movies/up"], Value::Number(0.0));
        assert_eq!(got["apps_up"], Value::Number(1.0));
        assert_eq!(got["apps_total"], Value::Number(2.0));
        assert_eq!(got["_endpoints_failed"], Value::Number(6.0));
        assert!(!got.contains_key("movies/queue"));
    }

    #[tokio::test]
    async fn a_wrong_key_fails_without_echoing_it() {
        let server = seerr().await;
        let err =
            format!("{:#}", plugin(&[("requests", "seerr", &server)], "wrong-key-value").poll().await.unwrap_err());
        assert!(err.contains("404") && !err.contains("wrong-key-value"), "{err}");
    }

    #[test]
    fn defaults_and_validation() {
        let one = "apps: [{name: movies, kind: radarr, url: 'https://radarr.example.invalid', api_key: k}]";
        let s: Settings = serde_yaml::from_str(one).unwrap();
        assert!(s.timeout_s.is_none() && !s.apps[0].insecure);
        assert_eq!(Servarr::new("m", s).unwrap().interval(), Duration::from_secs(60));
        let none: Settings = serde_yaml::from_str("apps: []").unwrap();
        assert!(Servarr::new("m", none).is_err());
        let teapot = "apps: [{name: x, kind: teapot, url: 'https://x.example.invalid', api_key: k}]";
        assert!(serde_yaml::from_str::<Settings>(teapot).is_err());
    }
}
