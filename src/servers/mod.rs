//! Server monitoring & inventory: the servers listed in `SCRIPTS_DIR/servers/servers.conf`,
//! their collected system information, history, alerts and the Servers page.
//!
//! - `GET  /servers` lists every server with its status, summary and recent trend
//! - `GET  /servers/{server_id}` returns one server with the full collected data
//! - `GET  /servers/{server_id}/history` returns CPU/RAM/disk history for trend charts
//! - `GET  /servers/{server_id}/containers/{container}` inspects one container (read-only)
//! - `GET  /servers/{server_id}/units/{unit}` shows one systemd unit with its recent journal
//! - `POST /servers/refresh` starts collecting all servers in the background
//! - `POST /servers/{server_id}/refresh` starts collecting one server in the background
//! - `GET  /servers/alerts` lists recent alerts; `GET /servers/audit` the refresh/lookup log
//! - `GET  /servers/dashboard` serves the Servers page (public; it asks for the token)
//!
//! Collection is done by `SCRIPTS_DIR/servers/collect-server.sh <server id>`, which runs the
//! read-only `remote-probe.sh` on the host over ssh and prints one JSON object. Only ids from
//! the inventory are accepted, so a request can never reach an arbitrary host or command.
//! Results, history, alerts and the audit log are kept in memory and, when `SERVERS_DATA_DIR`
//! is set (default `data`), persisted there as JSON / JSON Lines.

pub mod alerts;
pub mod detail;
mod store;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    process::Output,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{ConnectInfo, FromRequestParts, Path, Query, State},
    http::{StatusCode, header, request::Parts},
    response::{Html, IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;
use utoipa::{IntoParams, ToSchema};

use crate::{AppState, scripts::run_script_with_args};
use alerts::{AlertEvent, AlertKind};
use store::Store;

/// Folder (inside `SCRIPTS_DIR`) holding the server scripts and the inventory.
pub const SERVERS_DIR: &str = "servers";

/// Inventory file inside [`SERVERS_DIR`]: `<id> <host> <port> <user> <name...>` per line.
pub const INVENTORY_FILE: &str = "servers.conf";

/// Script inside [`SERVERS_DIR`] that collects one server, given its id as `$1`.
pub const COLLECT_SCRIPT: &str = "collect-server.sh";

/// Script inside [`SERVERS_DIR`] that posts one alert (JSON in `$1`) to the webhook.
pub const ALERT_SCRIPT: &str = "send-alert.sh";

/// Ids that would clash with the static routes under `/servers/`.
const RESERVED_IDS: [&str; 4] = ["refresh", "dashboard", "alerts", "audit"];

/// Probe sections whose absence does not make a server "partial" (missing on most VMs).
const OPTIONAL_SECTIONS: [&str; 1] = ["temperatures"];

/// Longest error message kept from the collector.
const MAX_ERROR_LEN: usize = 500;

/// History points returned with each server in `GET /servers` (card sparklines).
const RECENT_POINTS: usize = 24;

/// Upper bound of history points kept per server, whatever the retention.
const MAX_HISTORY_POINTS: usize = 20_000;

/// History responses are averaged down to at most this many points.
const MAX_CHART_POINTS: usize = 720;

/// Alerts and audit entries kept in memory (and read back from disk at startup).
const MAX_ALERTS: usize = 200;
const MAX_AUDIT: usize = 300;

/// How often old history is pruned from memory and the history file.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// Servers page, embedded at compile time so the binary needs no runtime files.
const SERVERS_HTML: &str = include_str!("../../templates/servers.html");

/// Settings of the Servers feature, from `SERVERS_*` environment variables.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Folder for persisted results, history, alerts and audit log (`SERVERS_DATA_DIR`,
    /// default `data`; `off` disables persistence).
    pub data_dir: Option<PathBuf>,
    /// Days of history to keep (`SERVERS_HISTORY_DAYS`, default 30).
    pub history_days: u32,
    /// Warning threshold in percent (`SERVERS_WARNING_PERCENT`, default 80).
    pub warning_percent: f64,
    /// Critical threshold in percent (`SERVERS_CRITICAL_PERCENT`, default 90).
    pub critical_percent: f64,
    /// Servers collected at the same time (`SERVERS_CONCURRENCY`, default 8).
    pub concurrency: usize,
    /// Minimum seconds between refreshes of all servers, or of one server
    /// (`SERVERS_REFRESH_MIN_INTERVAL`, default 10; 0 disables the limit).
    pub refresh_min_interval_secs: u64,
    /// Webhook for alerts (`SERVERS_ALERT_WEBHOOK_URL`; unset = no outgoing calls).
    pub alert_webhook_url: Option<String>,
    /// Seconds before the same alert for the same server is sent again (`SERVERS_ALERT_COOLDOWN`, default 3600).
    pub alert_cooldown_secs: u64,
}

impl Default for Settings {
    /// Defaults without persistence (used by tests); [`Settings::from_lookup`] defaults `data_dir` to `data`.
    fn default() -> Self {
        Self {
            data_dir: None,
            history_days: 30,
            warning_percent: 80.0,
            critical_percent: 90.0,
            concurrency: 8,
            refresh_min_interval_secs: 10,
            alert_webhook_url: None,
            alert_cooldown_secs: 3600,
        }
    }
}

impl Settings {
    /// Reads the `SERVERS_*` variables; returns a readable error for invalid values.
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = Self::default();
        let value = |key: &str| {
            get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let float = |key: &str, default: f64| -> Result<f64, String> {
            value(key).map_or(Ok(default), |v| {
                v.parse::<f64>()
                    .ok()
                    .filter(|x| (1.0..=100.0).contains(x))
                    .ok_or_else(|| format!("{key} must be a number between 1 and 100"))
            })
        };
        let int = |key: &str, default: u64, min: u64, max: u64| -> Result<u64, String> {
            value(key).map_or(Ok(default), |v| {
                v.parse::<u64>()
                    .ok()
                    .filter(|x| (min..=max).contains(x))
                    .ok_or_else(|| format!("{key} must be a whole number between {min} and {max}"))
            })
        };
        let data_dir = match get("SERVERS_DATA_DIR") {
            None => Some(PathBuf::from("data")),
            Some(v) => {
                let v = v.trim();
                if v.is_empty()
                    || matches!(v.to_ascii_lowercase().as_str(), "off" | "none" | "false")
                {
                    None
                } else {
                    Some(PathBuf::from(v))
                }
            }
        };
        let warning_percent = float("SERVERS_WARNING_PERCENT", d.warning_percent)?;
        let critical_percent = float("SERVERS_CRITICAL_PERCENT", d.critical_percent)?;
        if warning_percent >= critical_percent {
            return Err(
                "SERVERS_WARNING_PERCENT must be lower than SERVERS_CRITICAL_PERCENT".into(),
            );
        }
        let alert_webhook_url = value("SERVERS_ALERT_WEBHOOK_URL");
        if let Some(url) = &alert_webhook_url
            && (!(url.starts_with("http://") || url.starts_with("https://"))
                || url.chars().any(char::is_whitespace))
        {
            return Err("SERVERS_ALERT_WEBHOOK_URL must be an http:// or https:// URL".into());
        }
        Ok(Self {
            data_dir,
            history_days: u32::try_from(int("SERVERS_HISTORY_DAYS", 30, 1, 3650)?).unwrap_or(30),
            warning_percent,
            critical_percent,
            concurrency: usize::try_from(int("SERVERS_CONCURRENCY", 8, 1, 64)?).unwrap_or(8),
            refresh_min_interval_secs: int("SERVERS_REFRESH_MIN_INTERVAL", 10, 0, 3600)?,
            alert_webhook_url,
            alert_cooldown_secs: int("SERVERS_ALERT_COOLDOWN", 3600, 0, 604_800)?,
        })
    }

    fn thresholds(&self) -> Thresholds {
        Thresholds {
            warning_percent: self.warning_percent,
            critical_percent: self.critical_percent,
        }
    }
}

/// One server of the inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ServerEntry {
    /// Inventory id, e.g. `server-1`.
    #[schema(example = "server-1")]
    pub id: String,
    /// Display name.
    #[schema(example = "Gateway/Proxy")]
    pub name: String,
    /// IP address or host name.
    #[schema(example = "192.168.0.121")]
    pub host: String,
    /// ssh port.
    pub port: u16,
    /// ssh user (used by the script, not returned by the API).
    #[serde(skip)]
    pub user: String,
}

/// Availability of a server, from its latest collection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    /// Never collected.
    #[default]
    Unknown,
    /// Reached and every metric was collected.
    Online,
    /// Reached, but some metrics could not be collected (see `unavailable`).
    Partial,
    /// Could not be reached (ssh failed or timed out); `last_error` says why.
    Offline,
}

/// Resource level from the highest of CPU, RAM and (local) disk usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Below the warning threshold.
    Ok,
    /// At or above the warning threshold.
    Warning,
    /// At or above the critical threshold.
    Critical,
    /// No usage data.
    Unknown,
}

/// Key figures from the last successful collection, for the server cards.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ServerSummary {
    /// Host name reported by the server.
    pub hostname: Option<String>,
    /// Operating system, e.g. `Ubuntu 22.04.4 LTS`.
    pub os: Option<String>,
    /// CPU usage over a 1 second sample, in percent.
    pub cpu_percent: Option<f64>,
    /// RAM in use (total minus available), in percent.
    pub memory_percent: Option<f64>,
    /// Usage of the fullest local partition, in percent (network shares are excluded).
    pub disk_percent: Option<f64>,
    /// Mount point of the fullest local partition.
    pub disk_mount: Option<String>,
    /// Number of mounted network shares (cifs, nfs, …), not counted in `disk_percent`.
    pub network_shares: usize,
    /// Swap in use, in percent.
    pub swap_percent: Option<f64>,
    /// 1-minute load average.
    pub load_1: Option<f64>,
    /// Logical CPU count.
    pub logical_cores: Option<u64>,
    /// Seconds since boot.
    pub uptime_seconds: Option<u64>,
    /// Running Docker containers, when Docker is readable.
    pub containers_running: Option<u64>,
    /// Level derived from CPU, RAM and local disk usage.
    pub level: Level,
}

/// One point of a server's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct HistoryPoint {
    /// When the collection finished (Unix seconds).
    pub t: i64,
    /// Status of that collection.
    pub status: ServerStatus,
    /// CPU usage in percent.
    pub cpu: Option<f64>,
    /// RAM usage in percent.
    pub mem: Option<f64>,
    /// Fullest local disk in percent.
    pub disk: Option<f64>,
    /// 1-minute load average.
    pub load_1: Option<f64>,
}

/// One server as returned by the API.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ServerView {
    /// Inventory id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// IP address or host name.
    pub host: String,
    /// ssh port.
    pub port: u16,
    /// Availability from the latest collection.
    pub status: ServerStatus,
    /// `true` while a collection of this server is running or queued.
    pub collecting: bool,
    /// When the latest collection started (Unix seconds).
    pub last_attempt_at: Option<i64>,
    /// When the latest successful collection finished (Unix seconds).
    pub last_success_at: Option<i64>,
    /// Why the latest collection failed, when it did.
    pub last_error: Option<String>,
    /// How long the latest collection took, in milliseconds.
    pub duration_ms: Option<u64>,
    /// Metric sections that could not be collected in the last successful collection.
    pub unavailable: Vec<String>,
    /// `true` when the data shown is from an earlier collection because the latest one failed.
    pub stale: bool,
    /// Key figures from the last successful collection.
    pub summary: Option<ServerSummary>,
    /// Latest history points, oldest first (only in `GET /servers`).
    pub recent: Vec<HistoryPoint>,
    /// Full probe output (only in `GET /servers/{server_id}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub data: Option<Value>,
}

/// Progress of a "refresh all" run.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct JobStatus {
    /// Increasing id of the run.
    pub id: u64,
    /// When it started (Unix seconds).
    pub started_at: i64,
    /// `true` until every server of the run has finished.
    pub running: bool,
    /// Number of servers in the run.
    pub total: usize,
    /// Servers finished so far.
    pub done: usize,
    /// Ids of the servers in the run.
    pub server_ids: Vec<String>,
}

/// Usage thresholds used for [`Level`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct Thresholds {
    /// Warning at or above this percentage.
    pub warning_percent: f64,
    /// Critical at or above this percentage.
    pub critical_percent: f64,
}

/// Response of `GET /servers`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ServerList {
    /// Servers in inventory order.
    pub servers: Vec<ServerView>,
    /// The latest "refresh all" run, if any.
    pub job: Option<JobStatus>,
    /// Thresholds used for `summary.level` and alerts.
    pub thresholds: Thresholds,
    /// Inventory lines that were skipped, with the reason.
    pub inventory_errors: Vec<String>,
    /// `true` when results and history are saved to `SERVERS_DATA_DIR`.
    pub persisted: bool,
    /// Days of history kept.
    pub history_days: u32,
    /// `true` when alerts are posted to a webhook.
    pub alerts_webhook: bool,
    /// Servers collected at the same time.
    pub concurrency: usize,
    /// Minimum seconds between refresh requests.
    pub refresh_min_interval_secs: u64,
}

/// Query of `GET /servers/{server_id}/history`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct HistoryQuery {
    /// Hours of history to return (default 24, at most the retention).
    pub hours: Option<u32>,
}

/// Response of `GET /servers/{server_id}/history`.
#[derive(Debug, Serialize, ToSchema)]
pub struct HistoryResponse {
    /// Server id.
    pub id: String,
    /// Hours covered.
    pub hours: u32,
    /// Points, oldest first (averaged when there are many).
    pub points: Vec<HistoryPoint>,
}

/// One refresh or lookup request, for the audit log.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AuditEntry {
    /// When (Unix seconds).
    pub at: i64,
    /// `refresh_all`, `refresh`, `container_detail` or `unit_detail`.
    pub action: String,
    /// Server id, when the action targets one server.
    pub server_id: Option<String>,
    /// Container or unit name for lookups.
    pub target: Option<String>,
    /// `started`, `already_running`, `rate_limited`, `ok` or an error.
    pub result: String,
    /// Client IP address as seen by the server.
    pub client: Option<String>,
    /// `X-Forwarded-For` header (set by a proxy such as Traefik).
    pub forwarded_for: Option<String>,
    /// `User-Agent` header.
    pub user_agent: Option<String>,
}

/// Response of `GET /servers/audit`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuditList {
    /// Entries, newest first.
    pub entries: Vec<AuditEntry>,
}

/// Errors of the servers routes.
#[derive(Debug)]
pub enum ApiError {
    /// Any status with a plain-text message.
    Status(StatusCode, String),
    /// `429` with `Retry-After` seconds.
    RateLimited(u64),
}

impl From<(StatusCode, String)> for ApiError {
    fn from((code, message): (StatusCode, String)) -> Self {
        Self::Status(code, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Status(code, message) => (code, message).into_response(),
            Self::RateLimited(secs) => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, secs.to_string())],
                format!("too many refresh requests; retry in {secs}s"),
            )
                .into_response(),
        }
    }
}

/// Result type of the servers routes.
pub type ApiResult<T> = Result<T, ApiError>;

/// Who sent a request, for the audit log. Never rejects.
#[derive(Debug, Clone, Default)]
pub struct Client {
    ip: Option<String>,
    forwarded_for: Option<String>,
    user_agent: Option<String>,
}

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.chars().take(200).collect::<String>())
        };
        Ok(Self {
            ip: parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip().to_string()),
            forwarded_for: header("x-forwarded-for"),
            user_agent: header("user-agent"),
        })
    }
}

/// Shared state of the Servers feature: settings, collection state, storage and the concurrency limit.
#[derive(Debug)]
pub struct Servers {
    /// Settings from the environment.
    pub settings: Settings,
    state: Mutex<ServersState>,
    store: Store,
    permits: Semaphore,
}

/// In-memory collection state.
#[derive(Debug, Default)]
pub struct ServersState {
    records: HashMap<String, Record>,
    history: HashMap<String, VecDeque<HistoryPoint>>,
    job: Option<Job>,
    last_refresh_all: Option<Instant>,
    alerts: VecDeque<AlertEvent>,
    next_alert_id: u64,
    alert_sent: HashMap<(String, AlertKind), Instant>,
    audit: VecDeque<AuditEntry>,
    last_prune: Option<Instant>,
}

/// What is known about one server.
#[derive(Debug, Default)]
struct Record {
    status: ServerStatus,
    collecting: bool,
    started: Option<Instant>,
    finished: Option<Instant>,
    last_attempt_at: Option<i64>,
    last_success_at: Option<i64>,
    last_error: Option<String>,
    duration_ms: Option<u64>,
    data: Option<Value>,
}

/// A record as saved in `servers-state.json`.
#[derive(Debug, Serialize, Deserialize)]
struct SavedRecord {
    status: ServerStatus,
    last_attempt_at: Option<i64>,
    last_success_at: Option<i64>,
    last_error: Option<String>,
    duration_ms: Option<u64>,
    data: Option<Value>,
}

/// One line of `servers-history.jsonl`.
#[derive(Debug, Serialize, Deserialize)]
struct HistoryLine {
    id: String,
    #[serde(flatten)]
    point: HistoryPoint,
}

/// A "refresh all" run.
#[derive(Debug)]
struct Job {
    id: u64,
    started: Instant,
    started_at: i64,
    ids: Vec<String>,
}

/// Result of one collection.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Reached; the probe output.
    Reached {
        data: Value,
        duration_ms: Option<u64>,
    },
    /// Not reached, or the collector failed.
    Failed {
        error: String,
        duration_ms: Option<u64>,
    },
}

impl Servers {
    /// Creates the state and loads saved results, history, alerts and audit log from `data_dir`.
    pub fn new(settings: Settings) -> Self {
        let store = Store::new(settings.data_dir.clone());
        let mut state = ServersState::default();
        if let Some(saved) = store.read_json::<HashMap<String, SavedRecord>>(store::STATE_FILE) {
            for (id, r) in saved {
                state.records.insert(
                    id,
                    Record {
                        status: r.status,
                        last_attempt_at: r.last_attempt_at,
                        last_success_at: r.last_success_at,
                        last_error: r.last_error,
                        duration_ms: r.duration_ms,
                        data: r.data,
                        ..Record::default()
                    },
                );
            }
        }
        let cutoff = now_secs() - i64::from(settings.history_days) * 86_400;
        for line in store.read_lines::<HistoryLine>(store::HISTORY_FILE) {
            if line.point.t >= cutoff {
                state
                    .history
                    .entry(line.id)
                    .or_default()
                    .push_back(line.point);
            }
        }
        for points in state.history.values_mut() {
            points.make_contiguous().sort_by_key(|p| p.t);
            while points.len() > MAX_HISTORY_POINTS {
                points.pop_front();
            }
        }
        state.alerts = tail(store.read_lines(store::ALERTS_FILE), MAX_ALERTS);
        state.next_alert_id = state.alerts.iter().map(|a| a.id).max().unwrap_or(0) + 1;
        state.audit = tail(store.read_lines(store::AUDIT_FILE), MAX_AUDIT);
        state.last_prune = Some(Instant::now());
        let permits = Semaphore::new(settings.concurrency.max(1));
        let servers = Self {
            settings,
            state: Mutex::new(state),
            store,
            permits,
        };
        // Rewrite the files without expired entries.
        servers.save_history();
        servers
            .store
            .rewrite(store::ALERTS_FILE, || jsonl(&servers.lock().alerts));
        servers
            .store
            .rewrite(store::AUDIT_FILE, || jsonl(&servers.lock().audit));
        servers
    }

    /// Locks the state, recovering from a poisoned lock (the data stays usable).
    fn lock(&self) -> MutexGuard<'_, ServersState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds an audit entry in memory and on disk.
    fn audit(
        &self,
        client: &Client,
        action: &str,
        server_id: Option<&str>,
        target: Option<&str>,
        result: &str,
    ) {
        let entry = AuditEntry {
            at: now_secs(),
            action: action.to_string(),
            server_id: server_id.map(str::to_string),
            target: target.map(str::to_string),
            result: result.to_string(),
            client: client.ip.clone(),
            forwarded_for: client.forwarded_for.clone(),
            user_agent: client.user_agent.clone(),
        };
        {
            let mut state = self.lock();
            state.audit.push_back(entry.clone());
            while state.audit.len() > MAX_AUDIT {
                state.audit.pop_front();
            }
        }
        self.store.append(store::AUDIT_FILE, &entry);
    }

    /// Stores a finished collection: status, history point, alerts, and saves to disk.
    fn finish(self: &Arc<Self>, entry: &ServerEntry, outcome: Outcome, alert_script: PathBuf) {
        let thresholds = self.settings.thresholds();
        let (line, events, prune) = {
            let mut state = self.lock();
            let before =
                state
                    .records
                    .get(&entry.id)
                    .map_or((ServerStatus::Unknown, Level::Unknown), |r| {
                        (
                            r.status,
                            r.data
                                .as_ref()
                                .map_or(Level::Unknown, |d| summarize(d, thresholds).level),
                        )
                    });
            apply(&mut state, &entry.id, outcome);
            let record = &state.records[&entry.id];
            let status = record.status;
            let summary = record.data.as_ref().map(|d| summarize(d, thresholds));
            let error = record.last_error.clone();
            let reached = status != ServerStatus::Offline;
            let level = if reached {
                summary.as_ref().map_or(Level::Unknown, |s| s.level)
            } else {
                before.1
            };
            let pick = |f: fn(&ServerSummary) -> Option<f64>| {
                if reached {
                    summary.as_ref().and_then(f)
                } else {
                    None
                }
            };
            let point = HistoryPoint {
                t: now_secs(),
                status,
                cpu: pick(|s| s.cpu_percent),
                mem: pick(|s| s.memory_percent),
                disk: pick(|s| s.disk_percent),
                load_1: pick(|s| s.load_1),
            };
            let points = state.history.entry(entry.id.clone()).or_default();
            points.push_back(point.clone());
            while points.len() > MAX_HISTORY_POINTS {
                points.pop_front();
            }
            let mut events = Vec::new();
            for kind in alerts::transitions(before, (status, level)) {
                let event = alerts::new_event(
                    &mut state,
                    &self.settings,
                    entry,
                    kind,
                    summary.as_ref(),
                    error.as_deref(),
                );
                events.push(event);
            }
            let prune = state.last_prune.is_none_or(|t| t.elapsed() >= PRUNE_EVERY);
            if prune {
                let cutoff = now_secs() - i64::from(self.settings.history_days) * 86_400;
                for points in state.history.values_mut() {
                    while points.front().is_some_and(|p| p.t < cutoff) {
                        points.pop_front();
                    }
                }
                state.last_prune = Some(Instant::now());
            }
            (
                HistoryLine {
                    id: entry.id.clone(),
                    point,
                },
                events,
                prune,
            )
        };
        self.save_state();
        if prune {
            self.save_history();
        } else {
            self.store.append(store::HISTORY_FILE, &line);
        }
        for event in events {
            if event.delivery == alerts::PENDING {
                alerts::spawn_delivery(Arc::clone(self), alert_script.clone(), event);
            } else {
                self.store.append(store::ALERTS_FILE, &event);
            }
        }
    }

    /// Records the webhook result of an alert.
    fn delivered(&self, mut event: AlertEvent, delivery: String) {
        {
            let mut state = self.lock();
            if let Some(e) = state.alerts.iter_mut().find(|e| e.id == event.id) {
                e.delivery.clone_from(&delivery);
            }
        }
        event.delivery = delivery;
        self.store.append(store::ALERTS_FILE, &event);
    }

    /// Writes `servers-state.json` (last results of every server).
    fn save_state(&self) {
        self.store.rewrite(store::STATE_FILE, || {
            let state = self.lock();
            let saved: HashMap<&String, SavedRecord> = state
                .records
                .iter()
                .filter(|(_, r)| r.last_attempt_at.is_some())
                .map(|(id, r)| {
                    (
                        id,
                        SavedRecord {
                            status: r.status,
                            last_attempt_at: r.last_attempt_at,
                            last_success_at: r.last_success_at,
                            last_error: r.last_error.clone(),
                            duration_ms: r.duration_ms,
                            data: r.data.clone(),
                        },
                    )
                })
                .collect();
            serde_json::to_string(&saved).unwrap_or_default()
        });
    }

    /// Rewrites `servers-history.jsonl` from memory.
    fn save_history(&self) {
        self.store.rewrite(store::HISTORY_FILE, || {
            let state = self.lock();
            let mut out = String::new();
            for (id, points) in &state.history {
                for point in points {
                    let line = HistoryLine {
                        id: id.clone(),
                        point: point.clone(),
                    };
                    if let Ok(text) = serde_json::to_string(&line) {
                        out.push_str(&text);
                        out.push('\n');
                    }
                }
            }
            out
        });
    }
}

/// Last `max` items of a list, as a deque.
fn tail<T>(mut items: Vec<T>, max: usize) -> VecDeque<T> {
    if items.len() > max {
        items.drain(..items.len() - max);
    }
    items.into()
}

/// JSON Lines text of a list.
fn jsonl<T: Serialize>(items: &VecDeque<T>) -> String {
    items
        .iter()
        .filter_map(|i| serde_json::to_string(i).ok())
        .map(|l| l + "\n")
        .collect()
}

/// `GET /servers`
///
/// Lists the inventory servers with their status, the summary of their last successful
/// collection, recent history and the progress of the latest "refresh all" run.
#[utoipa::path(
    get,
    path = "/servers",
    responses(
        (status = 200, description = "Servers and their last results", body = ServerList),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 500, description = "Inventory file missing or unreadable"),
    )
)]
pub async fn list_servers(State(state): State<AppState>) -> ApiResult<Json<ServerList>> {
    let (entries, inventory_errors) = load_inventory(&state.config.scripts_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let s = &state.servers.settings;
    let thresholds = s.thresholds();
    let servers = state.servers.lock();
    Ok(Json(ServerList {
        servers: entries
            .iter()
            .map(|e| {
                let recent = servers.history.get(&e.id).map_or_else(Vec::new, |h| {
                    h.iter()
                        .skip(h.len().saturating_sub(RECENT_POINTS))
                        .cloned()
                        .collect()
                });
                view(e, servers.records.get(&e.id), false, thresholds, recent)
            })
            .collect(),
        job: servers
            .job
            .as_ref()
            .map(|j| job_status(j, &servers.records)),
        thresholds,
        inventory_errors,
        persisted: s.data_dir.is_some(),
        history_days: s.history_days,
        alerts_webhook: s.alert_webhook_url.is_some(),
        concurrency: s.concurrency,
        refresh_min_interval_secs: s.refresh_min_interval_secs,
    }))
}

/// `GET /servers/{server_id}`
///
/// One server with the full data of its last successful collection (`data`).
#[utoipa::path(
    get,
    path = "/servers/{server_id}",
    params(("server_id" = String, Path, description = "Inventory id", example = "server-1")),
    responses(
        (status = 200, description = "Server with full data", body = ServerView),
        (status = 400, description = "Invalid server id"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "Not in the inventory"),
        (status = 500, description = "Inventory file missing or unreadable"),
    )
)]
pub async fn get_server(
    State(state): State<AppState>,
    Path(server_id): Path<String>,
) -> ApiResult<Json<ServerView>> {
    let entry = find_entry(&state, &server_id)?;
    let thresholds = state.servers.settings.thresholds();
    let servers = state.servers.lock();
    Ok(Json(view(
        &entry,
        servers.records.get(&entry.id),
        true,
        thresholds,
        Vec::new(),
    )))
}

/// `GET /servers/{server_id}/history`
///
/// CPU, RAM, fullest-disk and load history of one server, oldest first. Long ranges are
/// averaged down to at most 720 points; a bucket's status is its worst status.
#[utoipa::path(
    get,
    path = "/servers/{server_id}/history",
    params(("server_id" = String, Path, description = "Inventory id", example = "server-1"), HistoryQuery),
    responses(
        (status = 200, description = "History points", body = HistoryResponse),
        (status = 400, description = "Invalid server id"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "Not in the inventory"),
    )
)]
pub async fn server_history(
    State(state): State<AppState>,
    Path(server_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<HistoryResponse>> {
    let entry = find_entry(&state, &server_id)?;
    let max_hours = state.servers.settings.history_days.saturating_mul(24);
    let hours = query.hours.unwrap_or(24).clamp(1, max_hours.max(1));
    let cutoff = now_secs() - i64::from(hours) * 3600;
    let points: Vec<HistoryPoint> = state
        .servers
        .lock()
        .history
        .get(&entry.id)
        .map(|h| h.iter().filter(|p| p.t >= cutoff).cloned().collect())
        .unwrap_or_default();
    Ok(Json(HistoryResponse {
        id: entry.id,
        hours,
        points: downsample(points, MAX_CHART_POINTS),
    }))
}

/// `POST /servers/refresh`
///
/// Starts collecting every inventory server in the background (at most `SERVERS_CONCURRENCY`
/// at a time) and returns right away with the run's progress (`202`). If a run is still
/// going it is returned instead. A new run within `SERVERS_REFRESH_MIN_INTERVAL` seconds of
/// the previous one is refused with `429` and `Retry-After`. Every request is audited.
#[utoipa::path(
    post,
    path = "/servers/refresh",
    responses(
        (status = 202, description = "Collection started (or already running)", body = JobStatus),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 429, description = "Refreshed too recently; see Retry-After"),
        (status = 500, description = "Inventory file missing or unreadable"),
    )
)]
pub async fn refresh_all(
    State(state): State<AppState>,
    client: Client,
) -> ApiResult<(StatusCode, Json<JobStatus>)> {
    let (entries, _) = load_inventory(&state.config.scripts_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let min = Duration::from_secs(state.servers.settings.refresh_min_interval_secs);
    let result: Result<(JobStatus, Vec<ServerEntry>, &str), u64> = {
        let mut servers = state.servers.lock();
        let running = servers
            .job
            .as_ref()
            .map(|j| job_status(j, &servers.records))
            .filter(|j| j.running);
        if let Some(job) = running {
            Ok((job, Vec::new(), "already_running"))
        } else if let Some(wait) = servers
            .last_refresh_all
            .and_then(|t| min.checked_sub(t.elapsed()))
        {
            Err(wait.as_secs() + 1)
        } else {
            servers.last_refresh_all = Some(Instant::now());
            let job = Job {
                id: servers.job.as_ref().map_or(1, |j| j.id + 1),
                started: Instant::now(),
                started_at: now_secs(),
                ids: entries.iter().map(|e| e.id.clone()).collect(),
            };
            let to_start = begin(&mut servers, &entries);
            let status = job_status(&job, &servers.records);
            servers.job = Some(job);
            Ok((status, to_start, "started"))
        }
    };
    match result {
        Ok((job, to_start, outcome)) => {
            state
                .servers
                .audit(&client, "refresh_all", None, None, outcome);
            spawn_collections(&state, to_start);
            Ok((StatusCode::ACCEPTED, Json(job)))
        }
        Err(wait) => {
            state
                .servers
                .audit(&client, "refresh_all", None, None, "rate_limited");
            Err(ApiError::RateLimited(wait))
        }
    }
}

/// `POST /servers/{server_id}/refresh`
///
/// Starts collecting one server in the background and returns it right away (`202`,
/// `collecting: true`). Refused with `429` when that server was collected less than
/// `SERVERS_REFRESH_MIN_INTERVAL` seconds ago. Every request is audited.
#[utoipa::path(
    post,
    path = "/servers/{server_id}/refresh",
    params(("server_id" = String, Path, description = "Inventory id", example = "server-1")),
    responses(
        (status = 202, description = "Collection started (or already running)", body = ServerView),
        (status = 400, description = "Invalid server id"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "Not in the inventory"),
        (status = 429, description = "Refreshed too recently; see Retry-After"),
        (status = 500, description = "Inventory file missing or unreadable"),
    )
)]
pub async fn refresh_server(
    State(state): State<AppState>,
    Path(server_id): Path<String>,
    client: Client,
) -> ApiResult<(StatusCode, Json<ServerView>)> {
    let entry = find_entry(&state, &server_id)?;
    let min = Duration::from_secs(state.servers.settings.refresh_min_interval_secs);
    let thresholds = state.servers.settings.thresholds();
    let result: Result<(ServerView, Vec<ServerEntry>, &str), u64> = {
        let mut servers = state.servers.lock();
        let record = servers.records.get(&entry.id);
        let wait = record
            .filter(|r| !r.collecting)
            .and_then(|r| r.started)
            .and_then(|t| min.checked_sub(t.elapsed()));
        if let Some(wait) = wait {
            Err(wait.as_secs() + 1)
        } else {
            let to_start = begin(&mut servers, std::slice::from_ref(&entry));
            let outcome = if to_start.is_empty() {
                "already_running"
            } else {
                "started"
            };
            Ok((
                view(
                    &entry,
                    servers.records.get(&entry.id),
                    false,
                    thresholds,
                    Vec::new(),
                ),
                to_start,
                outcome,
            ))
        }
    };
    match result {
        Ok((view, to_start, outcome)) => {
            state
                .servers
                .audit(&client, "refresh", Some(&entry.id), None, outcome);
            spawn_collections(&state, to_start);
            Ok((StatusCode::ACCEPTED, Json(view)))
        }
        Err(wait) => {
            state
                .servers
                .audit(&client, "refresh", Some(&entry.id), None, "rate_limited");
            Err(ApiError::RateLimited(wait))
        }
    }
}

/// `GET /servers/audit`
///
/// Recent refresh and lookup requests (who, when, result), newest first.
#[utoipa::path(
    get,
    path = "/servers/audit",
    responses(
        (status = 200, description = "Audit log", body = AuditList),
        (status = 401, description = "Missing or wrong x-security-token"),
    )
)]
pub async fn list_audit(State(state): State<AppState>) -> Json<AuditList> {
    let servers = state.servers.lock();
    Json(AuditList {
        entries: servers.audit.iter().rev().cloned().collect(),
    })
}

/// `GET /servers/dashboard` — serves the Servers page (public; it asks for the token).
pub async fn servers_dashboard() -> Html<&'static str> {
    Html(SERVERS_HTML)
}

/// Validates the id and looks it up in the inventory.
fn find_entry(state: &AppState, id: &str) -> Result<ServerEntry, ApiError> {
    if !is_valid_server_id(id) {
        return Err((StatusCode::BAD_REQUEST, "invalid server id".to_string()).into());
    }
    let (entries, _) = load_inventory(&state.config.scripts_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    entries
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no server '{id}'")).into())
}

/// Marks the given servers as collecting and returns those that were not already.
fn begin(servers: &mut ServersState, entries: &[ServerEntry]) -> Vec<ServerEntry> {
    let now = now_secs();
    entries
        .iter()
        .filter(|entry| {
            let record = servers.records.entry(entry.id.clone()).or_default();
            if record.collecting {
                return false;
            }
            record.collecting = true;
            record.started = Some(Instant::now());
            record.last_attempt_at = Some(now);
            true
        })
        .cloned()
        .collect()
}

/// Runs the collector for each server in its own task; at most `SERVERS_CONCURRENCY` run at once.
fn spawn_collections(state: &AppState, entries: Vec<ServerEntry>) {
    let dir = state.config.scripts_dir.join(SERVERS_DIR);
    for entry in entries {
        let (dir, servers) = (dir.clone(), Arc::clone(&state.servers));
        tokio::spawn(async move {
            let outcome = {
                let _permit = servers.permits.acquire().await.ok();
                collect(&dir.join(COLLECT_SCRIPT), &entry.id).await
            };
            servers.finish(&entry, outcome, dir.join(ALERT_SCRIPT));
        });
    }
}

/// Runs the collect script for one server and interprets its output.
async fn collect(script: &FsPath, id: &str) -> Outcome {
    if !script.is_file() {
        return Outcome::Failed {
            error: format!("missing script: {}", script.display()),
            duration_ms: None,
        };
    }
    interpret(run_script_with_args(script, &[id]).await)
}

/// Turns the collect script's result into an [`Outcome`].
fn interpret(result: Result<Output, String>) -> Outcome {
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            return Outcome::Failed {
                error: truncate(&error),
                duration_ms: None,
            };
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Option<Value> = serde_json::from_str(stdout.trim()).ok();
    let duration_ms = parsed
        .as_ref()
        .and_then(|v| v.get("duration_ms"))
        .and_then(Value::as_u64);
    match parsed {
        Some(mut v) if v.get("reachable") == Some(&Value::Bool(true)) => match v.get_mut("data") {
            Some(data) if data.is_object() => Outcome::Reached {
                data: data.take(),
                duration_ms,
            },
            _ => Outcome::Failed {
                error: "collector returned no data".into(),
                duration_ms,
            },
        },
        Some(v) if v.get("reachable") == Some(&Value::Bool(false)) => Outcome::Failed {
            error: truncate(
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unreachable"),
            ),
            duration_ms,
        },
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Outcome::Failed {
                error: truncate(&if stderr.is_empty() {
                    format!("collector exited with {} and invalid output", output.status)
                } else {
                    stderr
                }),
                duration_ms: None,
            }
        }
    }
}

/// Stores a finished collection in the record.
fn apply(servers: &mut ServersState, id: &str, outcome: Outcome) {
    let record = servers.records.entry(id.to_string()).or_default();
    record.collecting = false;
    record.finished = Some(Instant::now());
    match outcome {
        Outcome::Reached { data, duration_ms } => {
            record.status = if missing_sections(&data).is_empty() {
                ServerStatus::Online
            } else {
                ServerStatus::Partial
            };
            record.data = Some(data);
            record.last_success_at = Some(now_secs());
            record.last_error = None;
            record.duration_ms = duration_ms;
        }
        Outcome::Failed { error, duration_ms } => {
            // Keep the last good data so the page can still show it, marked as stale.
            record.status = ServerStatus::Offline;
            record.last_error = Some(error);
            record.duration_ms = duration_ms;
        }
    }
}

/// Unavailable sections that count as missing (optional ones like temperatures don't).
fn missing_sections(data: &Value) -> Vec<String> {
    unavailable(data)
        .into_iter()
        .filter(|s| !OPTIONAL_SECTIONS.contains(&s.as_str()))
        .collect()
}

/// All sections the probe listed as unavailable.
fn unavailable(data: &Value) -> Vec<String> {
    data.get("unavailable")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Builds the API view of a server; `full` includes the probe data.
fn view(
    entry: &ServerEntry,
    record: Option<&Record>,
    full: bool,
    thresholds: Thresholds,
    recent: Vec<HistoryPoint>,
) -> ServerView {
    let default = Record::default();
    let record = record.unwrap_or(&default);
    ServerView {
        id: entry.id.clone(),
        name: entry.name.clone(),
        host: entry.host.clone(),
        port: entry.port,
        status: record.status,
        collecting: record.collecting,
        last_attempt_at: record.last_attempt_at,
        last_success_at: record.last_success_at,
        last_error: record.last_error.clone(),
        duration_ms: record.duration_ms,
        unavailable: record.data.as_ref().map(unavailable).unwrap_or_default(),
        stale: record.status == ServerStatus::Offline && record.data.is_some(),
        summary: record.data.as_ref().map(|d| summarize(d, thresholds)),
        recent,
        data: if full { record.data.clone() } else { None },
    }
}

/// Progress of a run: servers of the run that finished after it started. A later
/// single-server refresh does not undo that, so a finished run stays finished.
fn job_status(job: &Job, records: &HashMap<String, Record>) -> JobStatus {
    let done = job
        .ids
        .iter()
        .filter(|id| {
            records
                .get(*id)
                .is_none_or(|r| r.finished.is_some_and(|f| f >= job.started))
        })
        .count();
    JobStatus {
        id: job.id,
        started_at: job.started_at,
        running: done < job.ids.len(),
        total: job.ids.len(),
        done,
        server_ids: job.ids.clone(),
    }
}

/// Extracts the card figures from the probe output. Network shares don't count as disks.
fn summarize(data: &Value, thresholds: Thresholds) -> ServerSummary {
    let f = |path: &str| data.pointer(path).and_then(Value::as_f64);
    let s = |path: &str| {
        data.pointer(path)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let disks: Vec<&Value> = data
        .get("disks")
        .and_then(Value::as_array)
        .map(|d| d.iter().collect())
        .unwrap_or_default();
    let is_network = |d: &Value| d.get("network").and_then(Value::as_bool).unwrap_or(false);
    let fullest = disks
        .iter()
        .filter(|d| !is_network(d))
        .filter_map(|d| Some((d.get("used_percent")?.as_f64()?, *d)))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    let cpu_percent = f("/cpu/usage_percent");
    let memory_percent = f("/memory/used_percent");
    let disk_percent = fullest.map(|(p, _)| p);
    ServerSummary {
        hostname: s("/system/fqdn").or_else(|| s("/system/hostname")),
        os: s("/system/os"),
        cpu_percent,
        memory_percent,
        disk_percent,
        disk_mount: fullest.and_then(|(_, d)| d.get("mount")?.as_str().map(str::to_string)),
        network_shares: disks.iter().filter(|d| is_network(d)).count(),
        swap_percent: f("/memory/swap_used_percent"),
        load_1: f("/health/load_1"),
        logical_cores: data.pointer("/cpu/logical_cores").and_then(Value::as_u64),
        uptime_seconds: data
            .pointer("/system/uptime_seconds")
            .and_then(Value::as_u64),
        containers_running: data
            .pointer("/docker/containers_running")
            .and_then(Value::as_u64),
        level: level_of(&[cpu_percent, memory_percent, disk_percent], thresholds),
    }
}

/// Level from the highest known usage percentage.
fn level_of(values: &[Option<f64>], thresholds: Thresholds) -> Level {
    match values.iter().flatten().copied().reduce(f64::max) {
        None => Level::Unknown,
        Some(v) if v >= thresholds.critical_percent => Level::Critical,
        Some(v) if v >= thresholds.warning_percent => Level::Warning,
        Some(_) => Level::Ok,
    }
}

/// Averages points into at most `max` buckets; a bucket keeps its last time and worst status.
fn downsample(points: Vec<HistoryPoint>, max: usize) -> Vec<HistoryPoint> {
    if points.len() <= max || max == 0 {
        return points;
    }
    let size = points.len().div_ceil(max);
    let avg = |chunk: &[HistoryPoint], f: fn(&HistoryPoint) -> Option<f64>| {
        let vals: Vec<f64> = chunk.iter().filter_map(f).collect();
        (!vals.is_empty())
            .then(|| (vals.iter().sum::<f64>() / vals.len() as f64 * 10.0).round() / 10.0)
    };
    let rank = |s: ServerStatus| match s {
        ServerStatus::Offline => 3,
        ServerStatus::Partial => 2,
        ServerStatus::Online => 1,
        ServerStatus::Unknown => 0,
    };
    points
        .chunks(size)
        .map(|chunk| HistoryPoint {
            t: chunk.last().map_or(0, |p| p.t),
            status: chunk
                .iter()
                .map(|p| p.status)
                .max_by_key(|s| rank(*s))
                .unwrap_or_default(),
            cpu: avg(chunk, |p| p.cpu),
            mem: avg(chunk, |p| p.mem),
            disk: avg(chunk, |p| p.disk),
            load_1: avg(chunk, |p| p.load_1),
        })
        .collect()
}

/// Reads and parses `SCRIPTS_DIR/servers/servers.conf`.
fn load_inventory(scripts_dir: &FsPath) -> Result<(Vec<ServerEntry>, Vec<String>), String> {
    let path: PathBuf = scripts_dir.join(SERVERS_DIR).join(INVENTORY_FILE);
    std::fs::read_to_string(&path)
        .map(|text| parse_inventory(&text))
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Parses inventory lines `<id> <host> <port> <user> <name...>`; `#` lines and blanks are
/// ignored. Invalid or duplicate lines are skipped and reported.
fn parse_inventory(text: &str) -> (Vec<ServerEntry>, Vec<String>) {
    let mut entries: Vec<ServerEntry> = Vec::new();
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let problem = match fields.as_slice() {
            [id, host, port, user, name @ ..] => {
                let port = port.parse::<u16>().ok().filter(|p| *p > 0);
                if !is_valid_server_id(id) {
                    Some("invalid or reserved id")
                } else if !is_valid_host(host) {
                    Some("invalid host")
                } else if !is_valid_user(user) {
                    Some("invalid user")
                } else if !seen.insert(id.to_string()) {
                    Some("duplicate id")
                } else if let Some(port) = port {
                    entries.push(ServerEntry {
                        id: id.to_string(),
                        name: if name.is_empty() {
                            id.to_string()
                        } else {
                            name.join(" ")
                        },
                        host: host.to_string(),
                        port,
                        user: user.to_string(),
                    });
                    None
                } else {
                    Some("invalid port")
                }
            }
            _ => Some("expected <id> <host> <port> <user> <name>"),
        };
        if let Some(problem) = problem {
            errors.push(format!("line {}: {problem}", index + 1));
        }
    }
    (entries, errors)
}

/// Lowercase letters, digits, `-` and `_` (max 32), not a reserved route name.
pub(crate) fn is_valid_server_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && !RESERVED_IDS.contains(&id)
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

/// IPv4 address or DNS name characters only, not starting with `-` (so ssh can't read it as an option).
fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

/// Unix user name: lowercase letter or `_` first, then lowercase letters, digits, `-`, `_`.
fn is_valid_user(user: &str) -> bool {
    let mut chars = user.chars();
    user.len() <= 32
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

/// Cuts long messages to [`MAX_ERROR_LEN`] characters.
fn truncate(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_ERROR_LEN {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(MAX_ERROR_LEN).collect::<String>())
    }
}

/// Current time in Unix seconds.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests;
