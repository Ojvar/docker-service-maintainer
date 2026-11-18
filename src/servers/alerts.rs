//! Alerts: a server going offline or coming back, and usage crossing the warning/critical
//! thresholds. Events are kept in memory (and in `servers-alerts.jsonl` when persisted) and,
//! only when `SERVERS_ALERT_WEBHOOK_URL` is set, posted as JSON by `send-alert.sh`.
//! An event fires once per change; the same kind for the same server is not posted again
//! within `SERVERS_ALERT_COOLDOWN` seconds.

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use super::{
    Level, ServerEntry, ServerStatus, ServerSummary, Servers, ServersState, Settings, now_secs,
    truncate,
};
use crate::{AppState, scripts::run_script_with_args};

/// `delivery` of an event waiting for the webhook.
pub(super) const PENDING: &str = "pending";

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AlertKind {
    /// The server became unreachable.
    Offline,
    /// The server is reachable again.
    Recovered,
    /// Usage reached the warning threshold.
    Warning,
    /// Usage reached the critical threshold.
    Critical,
    /// Usage dropped back below the warning threshold.
    Resolved,
}

/// One alert.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AlertEvent {
    /// Increasing id.
    pub id: u64,
    /// When (Unix seconds).
    pub at: i64,
    /// Server id.
    pub server_id: String,
    /// Server display name.
    pub server_name: String,
    /// Server host.
    pub host: String,
    /// What happened.
    pub kind: AlertKind,
    /// Human-readable message.
    pub message: String,
    /// CPU usage at the time, in percent.
    pub cpu_percent: Option<f64>,
    /// RAM usage at the time, in percent.
    pub memory_percent: Option<f64>,
    /// Fullest local disk at the time, in percent.
    pub disk_percent: Option<f64>,
    /// `not_configured` (no webhook), `pending`, `sent`, `suppressed (cooldown)` or `failed: …`.
    pub delivery: String,
}

/// Response of `GET /servers/alerts`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AlertList {
    /// `true` when alerts are posted to `SERVERS_ALERT_WEBHOOK_URL`.
    pub webhook_configured: bool,
    /// Seconds before the same alert for the same server is posted again.
    pub cooldown_secs: u64,
    /// Warning threshold in percent.
    pub warning_percent: f64,
    /// Critical threshold in percent.
    pub critical_percent: f64,
    /// Events, newest first.
    pub events: Vec<AlertEvent>,
}

/// `GET /servers/alerts`
///
/// Recent alerts (offline/recovered, warning/critical/resolved), newest first, with their webhook delivery result.
#[utoipa::path(
    get,
    path = "/servers/alerts",
    responses(
        (status = 200, description = "Recent alerts", body = AlertList),
        (status = 401, description = "Missing or wrong x-security-token"),
    )
)]
pub async fn list_alerts(State(state): State<AppState>) -> Json<AlertList> {
    let s = &state.servers.settings;
    let events = state.servers.lock().alerts.iter().rev().cloned().collect();
    Json(AlertList {
        webhook_configured: s.alert_webhook_url.is_some(),
        cooldown_secs: s.alert_cooldown_secs,
        warning_percent: s.warning_percent,
        critical_percent: s.critical_percent,
        events,
    })
}

/// Alerts for a change from `before` to `after` (status, level). While offline the level is
/// carried over, so a recovery doesn't re-alert an unchanged level.
pub(super) fn transitions(
    before: (ServerStatus, Level),
    after: (ServerStatus, Level),
) -> Vec<AlertKind> {
    let (was, was_level) = before;
    let (now, level) = after;
    let mut kinds = Vec::new();
    if now == ServerStatus::Offline && was != ServerStatus::Offline {
        kinds.push(AlertKind::Offline);
    }
    if was == ServerStatus::Offline && matches!(now, ServerStatus::Online | ServerStatus::Partial) {
        kinds.push(AlertKind::Recovered);
    }
    if now != ServerStatus::Offline {
        let high = |l: Level| matches!(l, Level::Warning | Level::Critical);
        match level {
            Level::Critical if was_level != Level::Critical => kinds.push(AlertKind::Critical),
            Level::Warning if !high(was_level) => kinds.push(AlertKind::Warning),
            Level::Ok if high(was_level) => kinds.push(AlertKind::Resolved),
            _ => {}
        }
    }
    kinds
}

/// Records a new event in `state` and decides its delivery (webhook, cooldown or none).
pub(super) fn new_event(
    state: &mut ServersState,
    settings: &Settings,
    entry: &ServerEntry,
    kind: AlertKind,
    summary: Option<&ServerSummary>,
    error: Option<&str>,
) -> AlertEvent {
    let pct = |v: Option<f64>| v.map_or("—".to_string(), |v| format!("{v:.0}%"));
    let usage = summary.map_or(String::new(), |s| {
        format!(
            "CPU {}, RAM {}, disk {}{}",
            pct(s.cpu_percent),
            pct(s.memory_percent),
            pct(s.disk_percent),
            s.disk_mount
                .as_deref()
                .map_or(String::new(), |m| format!(" ({m})"))
        )
    });
    let who = format!("{} ({})", entry.name, entry.host);
    let message = match kind {
        AlertKind::Offline => format!("{who} is unreachable: {}", error.unwrap_or("unknown error")),
        AlertKind::Recovered => format!("{who} is reachable again"),
        AlertKind::Warning => format!(
            "{who} usage reached warning (≥{:.0}%): {usage}",
            settings.warning_percent
        ),
        AlertKind::Critical => format!(
            "{who} usage reached critical (≥{:.0}%): {usage}",
            settings.critical_percent
        ),
        AlertKind::Resolved => format!(
            "{who} usage is back below {:.0}%: {usage}",
            settings.warning_percent
        ),
    };
    let key = (entry.id.clone(), kind);
    let cooldown = Duration::from_secs(settings.alert_cooldown_secs);
    let delivery = if settings.alert_webhook_url.is_none() {
        "not_configured".to_string()
    } else if state
        .alert_sent
        .get(&key)
        .is_some_and(|t| t.elapsed() < cooldown)
    {
        "suppressed (cooldown)".to_string()
    } else {
        state.alert_sent.insert(key, std::time::Instant::now());
        PENDING.to_string()
    };
    let event = AlertEvent {
        id: state.next_alert_id.max(1),
        at: now_secs(),
        server_id: entry.id.clone(),
        server_name: entry.name.clone(),
        host: entry.host.clone(),
        kind,
        message: truncate(&message),
        cpu_percent: summary.and_then(|s| s.cpu_percent),
        memory_percent: summary.and_then(|s| s.memory_percent),
        disk_percent: summary.and_then(|s| s.disk_percent),
        delivery,
    };
    state.next_alert_id = event.id + 1;
    state.alerts.push_back(event.clone());
    while state.alerts.len() > super::MAX_ALERTS {
        state.alerts.pop_front();
    }
    event
}

/// Webhook body. `text` makes it readable as-is by Slack/Mattermost-style incoming webhooks.
pub(super) fn payload(event: &AlertEvent) -> String {
    json!({
        "source": "service-maintainer",
        "event": event.kind,
        "text": event.message,
        "at": event.at,
        "server": {"id": event.server_id, "name": event.server_name, "host": event.host},
        "metrics": {"cpu_percent": event.cpu_percent, "memory_percent": event.memory_percent, "disk_percent": event.disk_percent},
    })
    .to_string()
}

/// Posts an event with `send-alert.sh` in the background and records the result.
pub(super) fn spawn_delivery(servers: Arc<Servers>, script: PathBuf, event: AlertEvent) {
    tokio::spawn(async move {
        let delivery = if !script.is_file() {
            format!("failed: missing script {}", script.display())
        } else {
            match run_script_with_args(&script, &[&payload(&event)]).await {
                Ok(o) if o.status.success() => "sent".to_string(),
                Ok(o) => format!(
                    "failed: {}",
                    truncate(String::from_utf8_lossy(&o.stderr).trim())
                ),
                Err(e) => format!("failed: {e}"),
            }
        };
        if delivery != "sent" {
            eprintln!(
                "servers: alert {} for {}: {delivery}",
                event.id, event.server_id
            );
        }
        servers.delivered(event, delivery);
    });
}
