//! Server monitoring & inventory: the servers listed in `SCRIPTS_DIR/servers/servers.conf`,
//! their last collected system information, background collection and the Servers page.
//!
//! - `GET  /servers` lists every server with its status and a summary of the last collection
//! - `GET  /servers/{server_id}` returns one server with the full collected data
//! - `POST /servers/refresh` starts collecting all servers in the background (concurrently)
//! - `POST /servers/{server_id}/refresh` starts collecting one server in the background
//! - `GET  /servers/dashboard` serves the Servers page (public; it asks for the token)
//!
//! Collection is done by `SCRIPTS_DIR/servers/collect-server.sh <server id>`, which runs the
//! read-only `remote-probe.sh` on the host over ssh and prints one JSON object. Only ids from
//! the inventory are accepted, so a request can never reach an arbitrary host or command.
//! Results are kept in memory until the server restarts.

use std::{
    collections::{HashMap, HashSet},
    path::{Path as FsPath, PathBuf},
    process::Output,
    sync::{Arc, Mutex, MutexGuard},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::Html,
};
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

use crate::{AppState, scripts::run_script_with_args};

/// Folder (inside `SCRIPTS_DIR`) holding the server scripts and the inventory.
pub const SERVERS_DIR: &str = "servers";

/// Inventory file inside [`SERVERS_DIR`]: `<id> <host> <port> <user> <name...>` per line.
pub const INVENTORY_FILE: &str = "servers.conf";

/// Script inside [`SERVERS_DIR`] that collects one server, given its id as `$1`.
pub const COLLECT_SCRIPT: &str = "collect-server.sh";

/// Usage (CPU, RAM or disk, in percent) at which a server is shown as a warning.
pub const WARNING_PERCENT: f64 = 80.0;

/// Usage (CPU, RAM or disk, in percent) at which a server is shown as critical.
pub const CRITICAL_PERCENT: f64 = 90.0;

/// Ids that would clash with the static routes under `/servers/`.
const RESERVED_IDS: [&str; 2] = ["refresh", "dashboard"];

/// Probe sections whose absence does not make a server "partial" (missing on most VMs).
const OPTIONAL_SECTIONS: [&str; 1] = ["temperatures"];

/// Longest error message kept from the collector.
const MAX_ERROR_LEN: usize = 500;

/// Servers page, embedded at compile time so the binary needs no runtime files.
const SERVERS_HTML: &str = include_str!("../templates/servers.html");

/// Collection state shared by all handlers and background tasks.
pub type SharedServers = Arc<Mutex<ServersState>>;

/// One server of the inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ServerEntry {
    /// Inventory id, e.g. `server-1`.
    #[schema(example = "server-1")]
    pub id: String,
    /// Display name.
    #[schema(example = "Server 1")]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    /// Never collected since the service started.
    #[default]
    Unknown,
    /// Reached and every metric was collected.
    Online,
    /// Reached, but some metrics could not be collected (see `unavailable`).
    Partial,
    /// Could not be reached (ssh failed or timed out); `last_error` says why.
    Offline,
}

/// Resource level from the highest of CPU, RAM and disk usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Below the warning threshold.
    Ok,
    /// At or above [`WARNING_PERCENT`].
    Warning,
    /// At or above [`CRITICAL_PERCENT`].
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
    /// Usage of the fullest partition, in percent.
    pub disk_percent: Option<f64>,
    /// Mount point of the fullest partition.
    pub disk_mount: Option<String>,
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
    /// Level derived from CPU, RAM and disk usage.
    pub level: Level,
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
    /// `true` while a collection of this server is running.
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
#[derive(Debug, Clone, Serialize, ToSchema)]
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
    /// Thresholds used for `summary.level`.
    pub thresholds: Thresholds,
    /// Inventory lines that were skipped, with the reason.
    pub inventory_errors: Vec<String>,
}

/// In-memory collection state.
#[derive(Debug, Default)]
pub struct ServersState {
    records: HashMap<String, Record>,
    job: Option<Job>,
}

/// What is known about one server.
#[derive(Debug, Default)]
struct Record {
    status: ServerStatus,
    collecting: bool,
    finished: Option<Instant>,
    last_attempt_at: Option<i64>,
    last_success_at: Option<i64>,
    last_error: Option<String>,
    duration_ms: Option<u64>,
    data: Option<Value>,
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

/// `GET /servers`
///
/// Lists the inventory servers with their status, the summary of their last successful
/// collection and the progress of the latest "refresh all" run. Collects nothing itself.
#[utoipa::path(
    get,
    path = "/servers",
    responses(
        (status = 200, description = "Servers and their last results", body = ServerList),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 500, description = "Inventory file missing or unreadable"),
    )
)]
pub async fn list_servers(
    State(state): State<AppState>,
) -> Result<Json<ServerList>, (StatusCode, String)> {
    let (entries, inventory_errors) = load_inventory(&state.config.scripts_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let servers = lock(&state.servers);
    Ok(Json(ServerList {
        servers: entries
            .iter()
            .map(|e| view(e, servers.records.get(&e.id), false))
            .collect(),
        job: servers
            .job
            .as_ref()
            .map(|j| job_status(j, &servers.records)),
        thresholds: Thresholds {
            warning_percent: WARNING_PERCENT,
            critical_percent: CRITICAL_PERCENT,
        },
        inventory_errors,
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
) -> Result<Json<ServerView>, (StatusCode, String)> {
    let entry = find_entry(&state, &server_id)?;
    let servers = lock(&state.servers);
    Ok(Json(view(&entry, servers.records.get(&entry.id), true)))
}

/// `POST /servers/refresh`
///
/// Starts collecting every inventory server in the background, all at once, and returns
/// right away with the run's progress (`202`). Poll `GET /servers` for results; `job`
/// shows how many servers have finished. Servers already being collected are not started
/// twice. If a run is still going, it is returned instead of starting a new one.
#[utoipa::path(
    post,
    path = "/servers/refresh",
    responses(
        (status = 202, description = "Collection started (or already running)", body = JobStatus),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 500, description = "Inventory file missing or unreadable"),
    )
)]
pub async fn refresh_all(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<JobStatus>), (StatusCode, String)> {
    let (entries, _) = load_inventory(&state.config.scripts_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let (job, to_start) = {
        let mut servers = lock(&state.servers);
        if let Some(job) = servers.job.as_ref() {
            let status = job_status(job, &servers.records);
            if status.running {
                return Ok((StatusCode::ACCEPTED, Json(status)));
            }
        }
        let id = servers.job.as_ref().map_or(1, |j| j.id + 1);
        servers.job = Some(Job {
            id,
            started: Instant::now(),
            started_at: now_secs(),
            ids: entries.iter().map(|e| e.id.clone()).collect(),
        });
        let to_start = begin(&mut servers, &entries);
        let job = servers
            .job
            .as_ref()
            .map(|j| job_status(j, &servers.records));
        (job, to_start)
    };
    spawn_collections(&state, to_start);
    let job = job.ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, "job missing".into()))?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

/// `POST /servers/{server_id}/refresh`
///
/// Starts collecting one server in the background and returns it right away (`202`,
/// `collecting: true`). Poll `GET /servers/{server_id}` for the result.
#[utoipa::path(
    post,
    path = "/servers/{server_id}/refresh",
    params(("server_id" = String, Path, description = "Inventory id", example = "server-1")),
    responses(
        (status = 202, description = "Collection started (or already running)", body = ServerView),
        (status = 400, description = "Invalid server id"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "Not in the inventory"),
        (status = 500, description = "Inventory file missing or unreadable"),
    )
)]
pub async fn refresh_server(
    State(state): State<AppState>,
    Path(server_id): Path<String>,
) -> Result<(StatusCode, Json<ServerView>), (StatusCode, String)> {
    let entry = find_entry(&state, &server_id)?;
    let (view, to_start) = {
        let mut servers = lock(&state.servers);
        let to_start = begin(&mut servers, std::slice::from_ref(&entry));
        (
            view(&entry, servers.records.get(&entry.id), false),
            to_start,
        )
    };
    spawn_collections(&state, to_start);
    Ok((StatusCode::ACCEPTED, Json(view)))
}

/// `GET /servers/dashboard` — serves the Servers page (public; it asks for the token).
pub async fn servers_dashboard() -> Html<&'static str> {
    Html(SERVERS_HTML)
}

/// Locks the shared state, recovering from a poisoned lock (the data stays usable).
fn lock(servers: &SharedServers) -> MutexGuard<'_, ServersState> {
    servers.lock().unwrap_or_else(|e| e.into_inner())
}

/// Validates the id and looks it up in the inventory.
fn find_entry(state: &AppState, id: &str) -> Result<ServerEntry, (StatusCode, String)> {
    if !is_valid_server_id(id) {
        return Err((StatusCode::BAD_REQUEST, "invalid server id".into()));
    }
    let (entries, _) = load_inventory(&state.config.scripts_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    entries
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no server '{id}'")))
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
            record.last_attempt_at = Some(now);
            true
        })
        .cloned()
        .collect()
}

/// Runs the collector for each server in its own task, so they run concurrently and a
/// dropped request does not stop them.
fn spawn_collections(state: &AppState, entries: Vec<ServerEntry>) {
    let script = state
        .config
        .scripts_dir
        .join(SERVERS_DIR)
        .join(COLLECT_SCRIPT);
    for entry in entries {
        let (script, servers) = (script.clone(), Arc::clone(&state.servers));
        tokio::spawn(async move {
            let outcome = collect(&script, &entry.id).await;
            apply(&mut lock(&servers), &entry.id, outcome);
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

/// Stores a finished collection.
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
fn view(entry: &ServerEntry, record: Option<&Record>, full: bool) -> ServerView {
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
        summary: record.data.as_ref().map(summarize),
        data: if full { record.data.clone() } else { None },
    }
}

/// Progress of a run: servers of the run that finished after it started.
fn job_status(job: &Job, records: &HashMap<String, Record>) -> JobStatus {
    let done = job
        .ids
        .iter()
        .filter(|id| {
            records
                .get(*id)
                .is_none_or(|r| !r.collecting && r.finished.is_some_and(|f| f >= job.started))
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

/// Extracts the card figures from the probe output.
fn summarize(data: &Value) -> ServerSummary {
    let f = |path: &str| data.pointer(path).and_then(Value::as_f64);
    let s = |path: &str| {
        data.pointer(path)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let fullest = data
        .get("disks")
        .and_then(Value::as_array)
        .and_then(|disks| {
            disks
                .iter()
                .filter_map(|d| Some((d.get("used_percent")?.as_f64()?, d)))
                .max_by(|a, b| a.0.total_cmp(&b.0))
        });
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
        swap_percent: f("/memory/swap_used_percent"),
        load_1: f("/health/load_1"),
        logical_cores: data.pointer("/cpu/logical_cores").and_then(Value::as_u64),
        uptime_seconds: data
            .pointer("/system/uptime_seconds")
            .and_then(Value::as_u64),
        containers_running: data
            .pointer("/docker/containers_running")
            .and_then(Value::as_u64),
        level: level_of(&[cpu_percent, memory_percent, disk_percent]),
    }
}

/// Level from the highest known usage percentage.
fn level_of(values: &[Option<f64>]) -> Level {
    match values.iter().flatten().copied().reduce(f64::max) {
        None => Level::Unknown,
        Some(v) if v >= CRITICAL_PERCENT => Level::Critical,
        Some(v) if v >= WARNING_PERCENT => Level::Warning,
        Some(_) => Level::Ok,
    }
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
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::process::ExitStatusExt;

    fn output(code: i32, stdout: &str, stderr: &str) -> Result<Output, String> {
        Ok(Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn parses_inventory_and_reports_bad_lines() {
        let text = "# comment\n\
                    server-1  192.168.0.121  10022  qeng  Server 1\n\
                    \n\
                    server-2 10.0.0.2 22 root\n\
                    Bad 10.0.0.3 22 qeng x\n\
                    server-3 -oProxy=x 22 qeng x\n\
                    server-4 10.0.0.4 0 qeng x\n\
                    server-1 10.0.0.5 22 qeng dup\n\
                    refresh 10.0.0.6 22 qeng x\n\
                    server-5 10.0.0.7\n";
        let (entries, errors) = parse_inventory(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            ServerEntry {
                id: "server-1".into(),
                name: "Server 1".into(),
                host: "192.168.0.121".into(),
                port: 10022,
                user: "qeng".into(),
            }
        );
        assert_eq!(entries[1].name, "server-2");
        assert_eq!(errors.len(), 6, "{errors:?}");
        assert!(errors[0].starts_with("line 5:"));
    }

    #[test]
    fn validates_server_ids() {
        for id in ["server-1", "s_2", "a"] {
            assert!(is_valid_server_id(id), "{id}");
        }
        let long = "a".repeat(33);
        for id in [
            "",
            "Server",
            "a/b",
            "../x",
            "a b",
            "a;rm",
            "refresh",
            "dashboard",
            long.as_str(),
        ] {
            assert!(!is_valid_server_id(id), "{id:?}");
        }
    }

    #[test]
    fn validates_hosts_and_users() {
        assert!(is_valid_host("192.168.0.9") && is_valid_host("node-1.qeng.ir"));
        assert!(
            !is_valid_host("-oProxyCommand=x") && !is_valid_host("a b") && !is_valid_host("a;b")
        );
        assert!(is_valid_user("qeng") && is_valid_user("_svc-1"));
        assert!(
            !is_valid_user("")
                && !is_valid_user("Root")
                && !is_valid_user("1a")
                && !is_valid_user("a@b")
        );
    }

    #[test]
    fn interprets_reachable_server() {
        let out = r#"{"id":"s","reachable":true,"duration_ms":1200,"data":{"unavailable":[]}}"#;
        assert_eq!(
            interpret(output(0, out, "")),
            Outcome::Reached {
                data: json!({"unavailable": []}),
                duration_ms: Some(1200)
            }
        );
    }

    #[test]
    fn interprets_unreachable_and_broken_output() {
        let out = r#"{"id":"s","reachable":false,"duration_ms":8000,"exit_code":255,"error":"Connection timed out"}"#;
        assert_eq!(
            interpret(output(0, out, "")),
            Outcome::Failed {
                error: "Connection timed out".into(),
                duration_ms: Some(8000)
            }
        );
        match interpret(output(64, "", "unknown server id: x\n")) {
            Outcome::Failed { error, .. } => assert_eq!(error, "unknown server id: x"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            interpret(output(0, "{not json", "")),
            Outcome::Failed { .. }
        ));
        assert!(matches!(
            interpret(Err("spawn failed".into())),
            Outcome::Failed { .. }
        ));
    }

    #[test]
    fn apply_sets_status_and_keeps_stale_data() {
        let mut state = ServersState::default();
        let entry = ServerEntry {
            id: "s".into(),
            name: "S".into(),
            host: "10.0.0.1".into(),
            port: 22,
            user: "qeng".into(),
        };
        assert_eq!(begin(&mut state, std::slice::from_ref(&entry)).len(), 1);
        assert!(
            begin(&mut state, std::slice::from_ref(&entry)).is_empty(),
            "not started twice"
        );

        apply(
            &mut state,
            "s",
            Outcome::Reached {
                data: json!({"unavailable": ["temperatures"]}),
                duration_ms: None,
            },
        );
        assert_eq!(
            state.records["s"].status,
            ServerStatus::Online,
            "temperatures are optional"
        );

        apply(
            &mut state,
            "s",
            Outcome::Reached {
                data: json!({"unavailable": ["docker"]}),
                duration_ms: None,
            },
        );
        assert_eq!(state.records["s"].status, ServerStatus::Partial);

        apply(
            &mut state,
            "s",
            Outcome::Failed {
                error: "timeout".into(),
                duration_ms: None,
            },
        );
        let v = view(&entry, state.records.get("s"), true);
        assert_eq!(v.status, ServerStatus::Offline);
        assert!(v.stale && v.data.is_some() && v.last_success_at.is_some());
        assert_eq!(v.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn summarizes_probe_data() {
        let data = json!({
            "system": {"hostname": "h", "fqdn": "h.example", "os": "Ubuntu", "uptime_seconds": 3600},
            "cpu": {"usage_percent": 12.5, "logical_cores": 8},
            "memory": {"used_percent": 85.0, "swap_used_percent": 1.0},
            "disks": [{"mount": "/", "used_percent": 40.0}, {"mount": "/data", "used_percent": 95.5}],
            "health": {"load_1": 0.5},
            "docker": {"containers_running": 3},
        });
        let s = summarize(&data);
        assert_eq!(s.hostname.as_deref(), Some("h.example"));
        assert_eq!(s.disk_percent, Some(95.5));
        assert_eq!(s.disk_mount.as_deref(), Some("/data"));
        assert_eq!(s.containers_running, Some(3));
        assert_eq!(s.level, Level::Critical);
        assert_eq!(summarize(&json!({})).level, Level::Unknown);
    }

    #[test]
    fn levels_follow_thresholds() {
        assert_eq!(level_of(&[Some(10.0), None]), Level::Ok);
        assert_eq!(level_of(&[Some(80.0), Some(5.0)]), Level::Warning);
        assert_eq!(level_of(&[None, Some(90.0)]), Level::Critical);
        assert_eq!(level_of(&[None]), Level::Unknown);
    }

    #[test]
    fn job_progress_counts_finished_servers() {
        let mut state = ServersState::default();
        let entries: Vec<ServerEntry> = ["a", "b"]
            .iter()
            .map(|id| ServerEntry {
                id: id.to_string(),
                name: id.to_string(),
                host: "h".into(),
                port: 22,
                user: "u".into(),
            })
            .collect();
        state.job = Some(Job {
            id: 1,
            started: Instant::now(),
            started_at: 0,
            ids: vec!["a".into(), "b".into()],
        });
        begin(&mut state, &entries);
        let job = state.job.as_ref().unwrap();
        assert_eq!(job_status(job, &state.records).done, 0);
        apply(
            &mut state,
            "a",
            Outcome::Failed {
                error: "x".into(),
                duration_ms: None,
            },
        );
        let status = job_status(state.job.as_ref().unwrap(), &state.records);
        assert_eq!((status.done, status.running), (1, true));
        apply(
            &mut state,
            "b",
            Outcome::Failed {
                error: "x".into(),
                duration_ms: None,
            },
        );
        assert!(!job_status(state.job.as_ref().unwrap(), &state.records).running);
    }

    #[test]
    fn truncates_long_errors() {
        let long = "x".repeat(MAX_ERROR_LEN + 10);
        assert_eq!(truncate(&long).chars().count(), MAX_ERROR_LEN + 1);
        assert_eq!(truncate("  short \n"), "short");
    }
}
