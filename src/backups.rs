//! MongoDB backups on the backup host: listing (`GET /backups`), restoring
//! (`POST /backups/restore`) and the backups dashboard page (`GET /backups/dashboard`).
//!
//! The work is done by two scripts in `SCRIPTS_DIR` that reach the host over ssh:
//! `list-backups.sh` and `restore-backup.sh <database> <backup>`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use axum::{Json, extract::State, http::StatusCode, response::Html};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    AppState,
    scripts::{run_script, run_script_with_args},
};

/// Script (inside `SCRIPTS_DIR`) that prints `<mtime>\t<size>\t<name>` per backup file.
pub const LIST_BACKUPS_SCRIPT: &str = "list-backups.sh";

/// Script (inside `SCRIPTS_DIR`) that restores a backup, given `$1` database and `$2` file name.
pub const RESTORE_BACKUP_SCRIPT: &str = "restore-backup.sh";

/// Longest accepted database name (MongoDB's limit is 64 bytes).
const MAX_DB_NAME_LEN: usize = 64;

/// Longest accepted backup file name.
const MAX_BACKUP_NAME_LEN: usize = 255;

/// Backups dashboard page, embedded at compile time so the binary needs no runtime files.
const BACKUPS_HTML: &str = include_str!("../templates/backups.html");

/// One backup file on the backup host.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct BackupFile {
    /// File name inside the backups folder.
    #[schema(example = "qeng_db-backup-2026-11-11_06-00-20.archive.gz")]
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Last modification time, in Unix seconds.
    pub modified: i64,
    /// `false` when the name has characters the restore route refuses.
    pub restorable: bool,
}

/// Response of `GET /backups`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BackupList {
    /// Backup files, newest first.
    pub backups: Vec<BackupFile>,
    /// `true` while a restore started by this server is still running.
    pub restore_running: bool,
}

/// Body of `POST /backups/restore`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RestoreRequest {
    /// Backup file name, as listed by `GET /backups`.
    #[schema(example = "qeng_db-backup-2026-11-11_06-00-20.archive.gz")]
    pub backup: String,
    /// Database to restore into (its data is overwritten).
    #[schema(example = "qeng_test")]
    pub database: String,
}

/// Response of `POST /backups/restore`: the same fields as a script run, plus what was restored.
#[derive(Debug, Serialize, ToSchema)]
pub struct RestoreResult {
    /// The restored backup file name.
    pub backup: String,
    /// The database it was restored into.
    pub database: String,
    /// `true` when the restore script exited with status 0.
    pub success: bool,
    /// Exit code, or `None` if the script was killed by a signal.
    pub exit_code: Option<i32>,
    /// Everything the script wrote to stdout.
    pub stdout: String,
    /// Everything the script wrote to stderr.
    pub stderr: String,
    /// How long the restore took, in milliseconds.
    pub duration_ms: u64,
}

/// `GET /backups`
///
/// Lists the backup files on the backup host (via `list-backups.sh`), newest first.
#[utoipa::path(
    get,
    path = "/backups",
    responses(
        (status = 200, description = "Backup list", body = BackupList),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 500, description = "List script missing or failed (e.g. ssh error)"),
    )
)]
pub async fn list_backups(
    State(state): State<AppState>,
) -> Result<Json<BackupList>, (StatusCode, String)> {
    let backups = backup_files(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(BackupList {
        backups,
        restore_running: state.restoring.load(Ordering::SeqCst),
    }))
}

/// `POST /backups/restore`
///
/// Restores a listed backup into `database` with `restore-backup.sh`. This overwrites
/// that database and can take minutes; no timeout is applied, and the restore keeps
/// running even if the client disconnects. Only one restore runs at a time.
///
/// A restore that runs but fails still returns `200`, with `success: false`.
#[utoipa::path(
    post,
    path = "/backups/restore",
    request_body = RestoreRequest,
    responses(
        (status = 200, description = "Restore finished", body = RestoreResult),
        (status = 400, description = "Invalid backup or database name"),
        (status = 401, description = "Missing or wrong x-security-token"),
        (status = 404, description = "Backup not in the current list"),
        (status = 409, description = "Another restore is already running"),
        (status = 422, description = "Malformed JSON body"),
        (status = 500, description = "Listing failed or restore script missing / could not start"),
    )
)]
pub async fn restore_backup(
    State(state): State<AppState>,
    Json(req): Json<RestoreRequest>,
) -> Result<Json<RestoreResult>, (StatusCode, String)> {
    if !is_valid_backup_name(&req.backup) {
        return Err((StatusCode::BAD_REQUEST, "invalid backup name".into()));
    }
    if !is_valid_db_name(&req.database) {
        return Err((StatusCode::BAD_REQUEST, "invalid database name".into()));
    }
    let guard = RestoreGuard::acquire(&state.restoring).ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            "a restore is already running".to_string(),
        )
    })?;

    let files = backup_files(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if !files.iter().any(|f| f.name == req.backup) {
        return Err((StatusCode::NOT_FOUND, format!("no backup '{}'", req.backup)));
    }

    let script = state.config.scripts_dir.join(RESTORE_BACKUP_SCRIPT);
    if !script.is_file() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("missing script: {}", script.display()),
        ));
    }

    // Run in its own task so a dropped request doesn't release the lock mid-restore.
    let (database, backup) = (req.database.clone(), req.backup.clone());
    let started = Instant::now();
    let output = tokio::spawn(async move {
        let _guard = guard;
        run_script_with_args(&script, &[&database, &backup]).await
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("restore task failed: {e}"),
        )
    })?
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(RestoreResult {
        backup: req.backup,
        database: req.database,
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))
}

/// `GET /backups/dashboard` — serves the backups page (public; it asks for the token).
pub async fn backups_dashboard() -> Html<&'static str> {
    Html(BACKUPS_HTML)
}

/// Holds the "restore running" flag; clears it when dropped.
struct RestoreGuard(Arc<AtomicBool>);

impl RestoreGuard {
    /// Sets the flag, or returns `None` if it was already set.
    fn acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(Arc::clone(flag)))
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Runs the list script and parses its output.
async fn backup_files(state: &AppState) -> Result<Vec<BackupFile>, String> {
    let script = state.config.scripts_dir.join(LIST_BACKUPS_SCRIPT);
    if !script.is_file() {
        return Err(format!("missing script: {}", script.display()));
    }
    let output = run_script(&script).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("list script exited with {}", output.status)
        } else {
            stderr
        });
    }
    Ok(parse_listing(&String::from_utf8_lossy(&output.stdout)))
}

/// Parses `<mtime>\t<size>\t<name>` lines (mtime may have a fraction), newest first.
/// Blank or malformed lines are skipped.
fn parse_listing(text: &str) -> Vec<BackupFile> {
    let mut files: Vec<BackupFile> = text.lines().filter_map(parse_line).collect();
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    files
}

/// Parses one listing line.
fn parse_line(line: &str) -> Option<BackupFile> {
    let mut parts = line.splitn(3, '\t');
    let modified = parts.next()?.trim().parse::<f64>().ok()?;
    let size = parts.next()?.trim().parse::<u64>().ok()?;
    let name = parts.next()?.trim_end_matches('\r');
    if name.is_empty() || !modified.is_finite() {
        return None;
    }
    Some(BackupFile {
        name: name.to_string(),
        size,
        modified: modified.trunc() as i64,
        restorable: is_valid_backup_name(name),
    })
}

/// Accepts letters, digits, `.`, `-` and `_` (max 255 chars), not starting with `.`,
/// so the name can never leave the backups folder.
pub(crate) fn is_valid_backup_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_BACKUP_NAME_LEN
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Accepts letters, digits, `-` and `_` (max 64 chars).
pub(crate) fn is_valid_db_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_DB_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_backup_names() {
        for name in [
            "qeng_db-backup-2026-11-11_06-00-20.archive.gz",
            "a",
            "x.tar.gz",
            "A-b_C.1",
        ] {
            assert!(is_valid_backup_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_unsafe_backup_names() {
        let too_long = "a".repeat(256);
        for name in [
            "",
            ".",
            "..",
            ".hidden",
            "../etc/passwd",
            "a/b",
            "a b",
            "a;rm",
            "$(id)",
            "ä.gz",
            too_long.as_str(),
        ] {
            assert!(!is_valid_backup_name(name), "{name:?} should be rejected");
        }
    }

    #[test]
    fn validates_db_names() {
        for name in ["qeng_test", "qeng-db", "DB1"] {
            assert!(is_valid_db_name(name), "{name} should be valid");
        }
        let too_long = "a".repeat(65);
        for name in ["", "a.b", "a b", "a/b", "a;rm", "$x", too_long.as_str()] {
            assert!(!is_valid_db_name(name), "{name:?} should be rejected");
        }
    }

    #[test]
    fn parses_listing_newest_first() {
        let text = "1700000000.5\t10\told.archive.gz\n\
                    not a line\n\
                    \n\
                    1800000000.25\t2048\tnew.archive.gz\r\n\
                    1750000000\t5\tbad name.gz\n\
                    x\t1\tnope.gz\n";
        let files = parse_listing(text);
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["new.archive.gz", "bad name.gz", "old.archive.gz"]);
        assert_eq!(
            files[0],
            BackupFile {
                name: "new.archive.gz".into(),
                size: 2048,
                modified: 1_800_000_000,
                restorable: true
            }
        );
        assert!(!files[1].restorable);
    }

    #[test]
    fn restore_guard_is_exclusive_and_released_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        let guard = RestoreGuard::acquire(&flag).expect("first acquire");
        assert!(RestoreGuard::acquire(&flag).is_none());
        drop(guard);
        assert!(!flag.load(Ordering::SeqCst));
        assert!(RestoreGuard::acquire(&flag).is_some());
    }
}
