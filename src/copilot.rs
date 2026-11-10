//! Read-only ingestion for GitHub Copilot CLI and Copilot Desktop session state.
//!
//! Both clients persist sessions under
//! `<copilot-home>/session-state/<session-id>/events.jsonl`, so this single
//! reader covers both and never yields the same session twice.
//!
//! Each JSONL record is decoded only to its envelope (`type`, `timestamp`, and
//! a borrowed raw `data` slice). The `data` payload is decoded only for the
//! lifecycle, model, and usage events handled below, and only into the fields
//! the monitor needs. Prompts, assistant text, reasoning, tool arguments and
//! results, summaries, and credentials are never deserialized into owned
//! values, retained, or emitted.
//!
//! Persisted lifecycle evidence is limited: `session.idle` and
//! `assistant.usage` are ephemeral and not written to disk, so an
//! `assistant.turn_end` never proves that a session is idle, and persisted turn
//! events alone cannot prove that a client process is still running.
//!
//! A session is reported running only when an unambiguously open
//! `assistant.turn_start` is backed by an `inuse.<pid>.lock` file in the same
//! session directory whose content names the same PID and that PID is a live
//! process. The lock alone only proves that a client has the session open; it
//! stays present while the client waits for user input and can outlive a
//! crashed client.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

use crate::kiro::process_is_alive;
use crate::model::ThreadState;
use crate::rollout::RolloutActivity;

/// `source_kind` for rows read from the shared CLI/Desktop session state.
pub const SOURCE_KIND: &str = "copilot_session_state";
/// Thread id namespace; `--thread` also accepts the raw session id.
pub const ID_PREFIX: &str = "copilot:";
pub const VSCODE_SOURCE_KIND: &str = "copilot_vscode_chat";

const MAX_EVENT_LINES: usize = 200_000;
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ACTIVITY: usize = 25;
const MAX_MODELS: usize = 64;
const MAX_MODEL_NAME_LEN: usize = 200;
const MAX_SESSION_ID_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSnapshotStatus {
    /// The shutdown snapshot is the last record and the log ended cleanly.
    Latest,
    /// Readable events (for example `session.resume`) follow the snapshot, so
    /// it does not describe current cumulative usage.
    Superseded,
    /// Unreadable or truncated records follow the snapshot.
    Uncertain,
    /// Complete, contiguous persisted per-call receipts prove this total.
    VerifiedReceipts,
}

/// Per-model cumulative counters from the latest `session.shutdown`, summed
/// across models. A field is `None` unless every model entry reports it.
/// No total is derived: Copilot does not define how the counters overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotTokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub models: Vec<String>,
    pub observed_at: Option<DateTime<Utc>>,
    pub status: UsageSnapshotStatus,
    /// Number of `session.shutdown` records in the log.
    pub shutdown_count: usize,
}

/// Counters from the latest valid persisted `session.usage_record`: one
/// request, never a session total. A field is `None` when the record omits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopilotLastRequestUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub observed_at: Option<DateTime<Utc>>,
    /// Unreadable records or invalid usage records follow this record, so a
    /// newer request may exist.
    pub followed_by_unverified_records: bool,
}

#[derive(Debug, Clone)]
pub struct CopilotThreadRecord {
    pub id: String,
    pub session_id: String,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub recency_at: Option<DateTime<Utc>>,
    pub cwd: Option<String>,
    pub source_kind: String,
    /// Latest requested/selected model name. Not proof of the upstream model.
    pub model: Option<String>,
    pub model_observed_at: Option<DateTime<Utc>>,
    /// Every model name observed in the session, in first-seen order.
    pub models: Vec<String>,
    pub state: ThreadState,
    pub state_detail: String,
    pub lifecycle_at: Option<DateTime<Utc>>,
    pub activity: Vec<RolloutActivity>,
    pub token_usage: Option<CopilotTokenUsage>,
    pub last_request_usage: Option<CopilotLastRequestUsage>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct CopilotSnapshot {
    pub threads: Vec<CopilotThreadRecord>,
    pub warnings: Vec<String>,
}

/// `--copilot-home`, then `COPILOT_HOME`, then `~/.copilot`.
pub fn resolve_copilot_home(cli_home: Option<&str>) -> Result<(PathBuf, bool)> {
    if let Some(home) = cli_home.filter(|home| !home.is_empty()) {
        return Ok((PathBuf::from(home), true));
    }
    if let Some(home) = std::env::var_os("COPILOT_HOME").filter(|home| !home.is_empty()) {
        return Ok((PathBuf::from(home), true));
    }
    let home = dirs::home_dir()
        .map(|path| path.join(".copilot"))
        .ok_or_else(|| anyhow::anyhow!("Unable to resolve home dir for default COPILOT_HOME"))?;
    Ok((home, false))
}

pub fn read_snapshot(home: &Path) -> CopilotSnapshot {
    let now = Utc::now();
    let mut snapshot = read_snapshot_at(home);
    snapshot
        .threads
        .extend(read_vscode_sessions(now, &mut snapshot.warnings));
    snapshot
}

/// Reads only the shared CLI/Desktop source, for deterministic event-log checks.
pub fn read_snapshot_at(home: &Path) -> CopilotSnapshot {
    let mut out = CopilotSnapshot::default();
    let root = home.join("session-state");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            out.warnings.push(format!(
                "Copilot session-state directory missing: {}",
                root.display()
            ));
            return out;
        }
        Err(err) => {
            out.warnings.push(format!(
                "Unable to list Copilot session-state {}: {err}",
                root.display()
            ));
            return out;
        }
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        // `file_type` does not follow symlinks.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            out.warnings.push(format!(
                "skipping symlinked Copilot session directory: {}",
                entry.path().display()
            ));
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(session_id) = name.to_str().filter(|id| is_safe_session_id(id)) else {
            continue;
        };
        let events = entry.path().join("events.jsonl");
        let Ok(metadata) = fs::symlink_metadata(&events) else {
            // Missing, or deleted after the directory was listed.
            continue;
        };
        if metadata.file_type().is_symlink() {
            out.warnings.push(format!(
                "skipping symlinked Copilot event log: {}",
                events.display()
            ));
            continue;
        }
        if !metadata.file_type().is_file() {
            continue;
        }
        candidates.push((session_id.to_string(), events, metadata.modified().ok()));
    }
    candidates.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    for (session_id, path, _) in candidates {
        if let Some(record) = read_session(&session_id, &path, &mut out.warnings) {
            out.threads.push(record);
        }
    }
    out
}

fn vscode_storage_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    #[cfg(target_os = "macos")]
    let base = home.join("Library/Application Support/Code/User/workspaceStorage");
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData/Roaming"))
        .join("Code/User/workspaceStorage");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = home.join(".config/Code/User/workspaceStorage");
    vec![base]
}

fn read_vscode_sessions(
    now: DateTime<Utc>,
    warnings: &mut Vec<String>,
) -> Vec<CopilotThreadRecord> {
    let mut records = Vec::new();
    for root in vscode_storage_roots() {
        let workspaces = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                warnings.push(format!(
                    "Unable to list VS Code chat storage {}: {err}",
                    root.display()
                ));
                continue;
            }
        };
        for workspace in workspaces.flatten() {
            let Ok(kind) = workspace.file_type() else {
                continue;
            };
            if !kind.is_dir() || kind.is_symlink() {
                continue;
            }
            let sessions_dir = workspace.path().join("chatSessions");
            records.extend(read_vscode_workspace(&sessions_dir, now, warnings));
        }
    }
    records
}

fn read_vscode_workspace(
    sessions_dir: &Path,
    now: DateTime<Utc>,
    warnings: &mut Vec<String>,
) -> Vec<CopilotThreadRecord> {
    let sessions = match fs::read_dir(sessions_dir) {
        Ok(sessions) => sessions,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => {
            warnings.push("Unable to scan a VS Code chat session directory".to_string());
            return Vec::new();
        }
    };
    let mut files = BTreeMap::<String, (PathBuf, bool)>::new();
    let (mut unreadable, mut oversized, mut malformed) = (0, 0, 0);
    for session in sessions.flatten() {
        let kind = match session.file_type() {
            Ok(kind) => kind,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if kind.is_symlink() {
            unreadable += 1;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let path = session.path();
        let is_json = path.extension().and_then(|e| e.to_str()) == Some("json");
        let is_jsonl = path.extension().and_then(|e| e.to_str()) == Some("jsonl");
        if !is_json && !is_jsonl {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let entry = files
            .entry(stem.to_string())
            .or_insert_with(|| (path.clone(), is_json));
        if is_jsonl {
            *entry = (path, false);
        }
    }
    let mut records = Vec::new();
    for (path, is_snapshot) in files.into_values() {
        match read_vscode_file_checked(&path, is_snapshot, now) {
            Ok(Some(record)) => records.push(record),
            Ok(None) => {}
            Err(VscodeReadError::Unreadable) => unreadable += 1,
            Err(VscodeReadError::Oversized) => oversized += 1,
            Err(VscodeReadError::Malformed) => malformed += 1,
        }
    }
    if unreadable + oversized + malformed > 0 {
        warnings.push(format!(
            "Skipped VS Code chat files: {unreadable} unreadable, {oversized} over size limits, {malformed} malformed; per-file limits are 16 MiB and 4 MiB per JSONL line"
        ));
    }
    records
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VscodeSession {
    session_id: Option<String>,
    #[serde(default)]
    requests: Vec<VscodeRequest>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VscodeRequest {
    agent: Option<VscodeAgent>,
    model_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct VscodeAgent {
    #[serde(rename = "extensionId")]
    extension_id: Option<VscodeExtensionId>,
}

#[derive(Debug, Default, Deserialize)]
struct VscodeExtensionId {
    value: Option<String>,
}

#[derive(Deserialize)]
struct VscodeMutation<'a> {
    kind: u8,
    #[serde(default)]
    k: Vec<serde_json::Value>,
    #[serde(borrow, default)]
    v: MaybeRaw<'a>,
    #[serde(default)]
    i: Option<usize>,
}

#[derive(Default)]
struct MaybeRaw<'a>(Option<&'a RawValue>);

impl<'de: 'a, 'a> Deserialize<'de> for MaybeRaw<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        <&'de RawValue>::deserialize(deserializer).map(|raw| Self(Some(raw)))
    }
}

#[cfg(test)]
fn read_vscode_file(
    path: &Path,
    snapshot: bool,
    now: DateTime<Utc>,
) -> Option<CopilotThreadRecord> {
    read_vscode_file_checked(path, snapshot, now).ok().flatten()
}

#[derive(Debug, Clone, Copy)]
enum VscodeReadError {
    Unreadable,
    Oversized,
    Malformed,
}

fn read_vscode_file_checked(
    path: &Path,
    snapshot: bool,
    now: DateTime<Utc>,
) -> Result<Option<CopilotThreadRecord>, VscodeReadError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| VscodeReadError::Unreadable)?;
    if metadata.file_type().is_symlink() {
        return Err(VscodeReadError::Unreadable);
    }
    if metadata.len() > 16 * 1024 * 1024 {
        return Err(VscodeReadError::Oversized);
    }
    let file = File::open(path).map_err(|_| VscodeReadError::Unreadable)?;
    let mut reader = BufReader::new(file);
    let mut session = VscodeSession::default();
    if snapshot {
        session = serde_json::from_reader(&mut reader).map_err(|_| VscodeReadError::Malformed)?;
    } else {
        let mut initialized = false;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = read_bounded_line(&mut reader, &mut line, MAX_LINE_BYTES)
                .map_err(|_| VscodeReadError::Unreadable)?;
            match read {
                LineRead::Eof => break,
                LineRead::Oversized => return Err(VscodeReadError::Oversized),
                LineRead::Line { .. } => {}
            }
            let text = std::str::from_utf8(&line).map_err(|_| VscodeReadError::Malformed)?;
            let text = text.trim_end_matches(['\n', '\r']);
            if text.trim().is_empty() {
                continue;
            }
            let Ok(op) = serde_json::from_str::<VscodeMutation<'_>>(text) else {
                return Err(VscodeReadError::Malformed);
            };
            let valid = match op.kind {
                0 if op.k.is_empty() => {
                    let Some(initial) =
                        op.v.0
                            .and_then(|raw| serde_json::from_str::<VscodeSession>(raw.get()).ok())
                    else {
                        return Err(VscodeReadError::Malformed);
                    };
                    session = initial;
                    initialized = true;
                    true
                }
                2 if initialized && valid_vscode_path(&op.k) => {
                    if op.k.as_slice() != [serde_json::Value::String("requests".into())] {
                        if op.k.first() == Some(&serde_json::Value::String("requests".into())) {
                            op.k.len() >= 3
                                && op.k[1]
                                    .as_u64()
                                    .is_some_and(|index| index < session.requests.len() as u64)
                                && op.v.0.is_some()
                        } else {
                            op.v.0.is_some()
                        }
                    } else {
                        if let Some(index) = op.i {
                            if index > session.requests.len() {
                                false
                            } else {
                                session.requests.truncate(index);
                                if let Some(raw) = op.v.0 {
                                    match serde_json::from_str::<Vec<VscodeRequest>>(raw.get()) {
                                        Ok(mut appended) => {
                                            session.requests.append(&mut appended);
                                            true
                                        }
                                        Err(_) => false,
                                    }
                                } else {
                                    true
                                }
                            }
                        } else if let Some(raw) = op.v.0 {
                            match serde_json::from_str::<Vec<VscodeRequest>>(raw.get()) {
                                Ok(mut appended) => {
                                    session.requests.append(&mut appended);
                                    true
                                }
                                Err(_) => false,
                            }
                        } else {
                            false
                        }
                    }
                }
                1 if initialized && valid_vscode_path(&op.k) => op.v.0.is_some_and(|raw| {
                    if op.k.first() != Some(&serde_json::Value::String("requests".into())) {
                        return true;
                    }
                    apply_vscode_request_mutation(
                        &mut session.requests,
                        &op.k[1..],
                        raw.get(),
                        false,
                    )
                }),
                3 if initialized && valid_vscode_path(&op.k) => {
                    if op.k.first() == Some(&serde_json::Value::String("requests".into())) {
                        apply_vscode_request_mutation(
                            &mut session.requests,
                            &op.k[1..],
                            "null",
                            true,
                        )
                    } else {
                        true
                    }
                }
                _ => false,
            };
            if !valid {
                return Err(VscodeReadError::Malformed);
            }
        }
        if !initialized {
            return Err(VscodeReadError::Malformed);
        }
    }
    let mut models = Vec::new();
    let mut latest_model = None;
    let mut confirmed_copilot = false;
    for request in &session.requests {
        let confirmed = request
            .agent
            .as_ref()
            .and_then(|agent| agent.extension_id.as_ref())
            .and_then(|extension| extension.value.as_deref())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("github.copilot-chat"));
        if !confirmed {
            continue;
        }
        confirmed_copilot = true;
        if let Some(model) = request
            .model_id
            .as_deref()
            .and_then(|m| clean_model_name(m.to_string()))
        {
            if !models.contains(&model) {
                models.push(model.clone());
            }
            latest_model = Some(model);
        }
    }
    if !confirmed_copilot {
        return Ok(None);
    }
    let session_id = session
        .session_id
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .ok_or(VscodeReadError::Malformed)?;
    let workspace_id = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or(VscodeReadError::Malformed)?;
    if !is_safe_session_id(&session_id) || !is_safe_session_id(workspace_id) {
        return Err(VscodeReadError::Malformed);
    }
    let id = format!("copilot-vscode:{workspace_id}:{session_id}");
    let filename_time = metadata.modified().ok().map(chrono::DateTime::<Utc>::from);
    let updated_at = filename_time.filter(|time| *time <= now);
    Ok(Some(CopilotThreadRecord {
        id,
        session_id,
        created_at: None,
        updated_at,
        recency_at: updated_at,
        cwd: None,
        source_kind: VSCODE_SOURCE_KIND.to_string(),
        model: latest_model,
        model_observed_at: None,
        models,
        state: ThreadState::Unknown,
        state_detail:
            "VS Code persisted chat data does not prove whether the session is running or idle"
                .to_string(),
        lifecycle_at: None,
        activity: Vec::new(),
        token_usage: None,
        last_request_usage: None,
        path: path.to_path_buf(),
    }))
}

fn apply_vscode_request_mutation(
    requests: &mut Vec<VscodeRequest>,
    path: &[serde_json::Value],
    value: &str,
    delete: bool,
) -> bool {
    if path.is_empty() {
        if delete {
            requests.clear();
            return true;
        }
        return serde_json::from_str::<Vec<VscodeRequest>>(value)
            .map(|replacement| *requests = replacement)
            .is_ok();
    }
    let Some(index) = path
        .first()
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as usize)
    else {
        return false;
    };
    if index >= requests.len() {
        return false;
    }
    if path.len() == 1 && delete {
        requests[index] = VscodeRequest::default();
        return true;
    }
    if path.len() == 1 {
        return serde_json::from_str::<VscodeRequest>(value)
            .map(|request| requests[index] = request)
            .is_ok();
    }
    if path.len() < 2 {
        return false;
    }
    let field = path[1].as_str().unwrap_or_default();
    let request = &mut requests[index];
    match (field, path.get(2).and_then(serde_json::Value::as_str)) {
        ("modelId", None) => {
            if delete || value == "null" {
                request.model_id = None;
                true
            } else {
                match serde_json::from_str::<Option<String>>(value) {
                    Ok(model) => {
                        request.model_id = model;
                        true
                    }
                    Err(_) => false,
                }
            }
        }
        ("agent", None) => {
            if delete || value == "null" {
                request.agent = None;
                true
            } else {
                match serde_json::from_str::<Option<VscodeAgent>>(value) {
                    Ok(agent) => {
                        request.agent = agent;
                        true
                    }
                    Err(_) => false,
                }
            }
        }
        ("agent", Some("extensionId")) if path.len() == 3 => {
            let agent = request.agent.get_or_insert_with(VscodeAgent::default);
            if delete || value == "null" {
                agent.extension_id = None;
                true
            } else {
                match serde_json::from_str::<Option<VscodeExtensionId>>(value) {
                    Ok(extension_id) => {
                        agent.extension_id = extension_id;
                        true
                    }
                    Err(_) => false,
                }
            }
        }
        ("agent", Some("extensionId"))
            if path.get(3).and_then(serde_json::Value::as_str) == Some("value") =>
        {
            let agent = request.agent.get_or_insert_with(VscodeAgent::default);
            let extension = agent
                .extension_id
                .get_or_insert_with(VscodeExtensionId::default);
            if delete || value == "null" {
                extension.value = None;
                true
            } else {
                match serde_json::from_str::<Option<String>>(value) {
                    Ok(value) => {
                        extension.value = value;
                        true
                    }
                    Err(_) => false,
                }
            }
        }
        _ => true,
    }
}

fn valid_vscode_path(path: &[serde_json::Value]) -> bool {
    path.iter()
        .all(|part| part.is_string() || part.as_u64().is_some())
}

fn read_session(
    session_id: &str,
    path: &Path,
    warnings: &mut Vec<String>,
) -> Option<CopilotThreadRecord> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            warnings.push(format!(
                "Unable to read Copilot event log {}: {err}",
                path.display()
            ));
            return None;
        }
    };
    let mut reader = BufReader::new(file);
    let mut acc = Accumulator::default();
    let mut line = Vec::new();
    let mut lines = 0_usize;
    loop {
        line.clear();
        let read = match read_bounded_line(&mut reader, &mut line, MAX_LINE_BYTES) {
            Ok(read) => read,
            Err(err) => {
                acc.incomplete = true;
                acc.receipt_ledger_invalid = true;
                warnings.push(format!(
                    "Copilot event log read error in {}: {err}",
                    path.display()
                ));
                break;
            }
        };
        let terminated = match read {
            LineRead::Eof => break,
            LineRead::Oversized => {
                lines += 1;
                acc.oversized += 1;
                acc.receipt_ledger_invalid = true;
                acc.unreadable_since_lifecycle = true;
                acc.mark_last_request_unverified();
                continue;
            }
            LineRead::Line { terminated } => terminated,
        };
        lines += 1;
        if lines > MAX_EVENT_LINES {
            acc.incomplete = true;
            acc.receipt_ledger_invalid = true;
            warnings.push(format!(
                "Copilot event log truncated after {MAX_EVENT_LINES} lines: {}",
                path.display()
            ));
            break;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            acc.mark_unreadable(terminated);
            continue;
        };
        let text = text.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Envelope<'_>>(text) {
            Ok(envelope) => acc.ingest(&envelope),
            Err(_) => acc.mark_unreadable(terminated),
        }
    }
    line.clear();
    acc.finish(session_id, path, warnings)
}

enum LineRead {
    Eof,
    Line { terminated: bool },
    Oversized,
}

/// Reads one `\n`-terminated line into `buf`, discarding (not buffering) any
/// line longer than `max` bytes so large message or tool payloads cannot
/// grow memory without bound.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> io::Result<LineRead> {
    let mut oversized = false;
    let mut read_any = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if available.is_empty() {
            return Ok(if !read_any {
                LineRead::Eof
            } else if oversized {
                LineRead::Oversized
            } else {
                LineRead::Line { terminated: false }
            });
        }
        read_any = true;
        let (used, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => (index + 1, true),
            None => (available.len(), false),
        };
        if !oversized {
            if buf.len() + used > max {
                oversized = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&available[..used]);
            }
        }
        reader.consume(used);
        if done {
            return Ok(if oversized {
                LineRead::Oversized
            } else {
                LineRead::Line { terminated: true }
            });
        }
    }
}

/// Only the envelope is decoded; `data` stays a borrowed raw slice.
#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<&'a RawValue>,
    #[serde(borrow, default)]
    timestamp: Option<&'a RawValue>,
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartData<'a> {
    #[serde(borrow, default)]
    session_id: Option<&'a RawValue>,
    #[serde(borrow, default)]
    start_time: Option<&'a RawValue>,
    #[serde(borrow, default)]
    selected_model: Option<&'a RawValue>,
    #[serde(borrow, default)]
    context: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
struct ContextData<'a> {
    #[serde(borrow, default)]
    cwd: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelChangeData<'a> {
    #[serde(borrow, default)]
    new_model: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnData<'a> {
    #[serde(borrow, default)]
    turn_id: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShutdownData<'a> {
    #[serde(borrow, default)]
    current_model: Option<&'a RawValue>,
    #[serde(borrow, default)]
    model_metrics: Option<&'a RawValue>,
    #[serde(borrow, default)]
    usage_accounting_watermarks: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
struct MetricData<'a> {
    #[serde(borrow, default)]
    usage: Option<&'a RawValue>,
}

/// Only `data.usage` of a `session.usage_record` is decoded; other fields are
/// skipped without being retained.
#[derive(Default, Deserialize)]
struct UsageRecordData<'a> {
    #[serde(borrow, default)]
    usage: Option<&'a RawValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UsageReceipt {
    source: String,
    sequence: u64,
    usage_id: String,
    input: u64,
    output: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageAccounting {
    source_session_id: Option<String>,
    sequence: Option<u64>,
    usage_id: Option<String>,
}

#[derive(Deserialize)]
struct UsageWatermarks(BTreeMap<String, u64>);

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageData<'a> {
    #[serde(borrow, default)]
    accounting: Option<&'a RawValue>,
    #[serde(borrow, default)]
    input_tokens: Option<&'a RawValue>,
    #[serde(borrow, default)]
    output_tokens: Option<&'a RawValue>,
    #[serde(borrow, default)]
    cache_read_tokens: Option<&'a RawValue>,
    #[serde(borrow, default)]
    cache_write_tokens: Option<&'a RawValue>,
    #[serde(borrow, default)]
    reasoning_tokens: Option<&'a RawValue>,
}

#[derive(Debug, Clone)]
struct UsageSnapshot {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
    models: Vec<String>,
    observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default)]
struct Accumulator {
    records: usize,
    start_seen: bool,
    start_session_id: Option<String>,
    identity_conflict: bool,
    start_time: Option<DateTime<Utc>>,
    first_at: Option<DateTime<Utc>>,
    last_at: Option<DateTime<Utc>>,
    lifecycle_at: Option<DateTime<Utc>>,
    cwd: Option<String>,
    model: Option<String>,
    model_at: Option<DateTime<Utc>>,
    models: Vec<String>,
    open_turns: HashSet<String>,
    turn_ids_ambiguous: bool,
    /// The latest readable record is `session.shutdown`.
    ended: bool,
    usage: Option<UsageSnapshot>,
    usage_superseded: bool,
    shutdown_count: usize,
    /// Latest valid `session.usage_record` counters (one request).
    last_request: Option<CopilotLastRequestUsage>,
    /// Unreadable or invalid records follow `last_request`.
    last_request_unverified: bool,
    receipt_by_sequence: BTreeMap<(String, u64), UsageReceipt>,
    receipt_by_id: HashMap<String, UsageReceipt>,
    receipt_ledger_invalid: bool,
    receipt_watermarks: Vec<BTreeMap<String, u64>>,
    usage_record_count: usize,
    receipt_events_without_accounting: usize,
    receipt_accounting_seen: bool,
    receipt_last_at: Option<DateTime<Utc>>,
    activity: VecDeque<RolloutActivity>,
    malformed: usize,
    malformed_data: usize,
    oversized: usize,
    truncated_tail: bool,
    incomplete: bool,
    /// An unreadable record follows the latest lifecycle event, so that event
    /// may not be the latest one.
    unreadable_since_lifecycle: bool,
}

impl Accumulator {
    fn mark_unreadable(&mut self, terminated: bool) {
        self.receipt_ledger_invalid = true;
        if terminated {
            self.malformed += 1;
        } else {
            self.truncated_tail = true;
        }
        self.unreadable_since_lifecycle = true;
        self.mark_last_request_unverified();
    }

    fn mark_last_request_unverified(&mut self) {
        if self.last_request.is_some() {
            self.last_request_unverified = true;
        }
    }

    fn ingest(&mut self, envelope: &Envelope<'_>) {
        self.records += 1;
        let ts = raw_string(envelope.timestamp)
            .as_deref()
            .and_then(parse_rfc3339);
        if let Some(ts) = ts {
            self.first_at = Some(self.first_at.map_or(ts, |first| first.min(ts)));
            self.last_at = Some(self.last_at.map_or(ts, |last| last.max(ts)));
        }
        // Any record after a shutdown means the session continued, so the
        // shutdown snapshot no longer describes the current session.
        if self.ended {
            self.ended = false;
            if self.usage.is_some() {
                self.usage_superseded = true;
            }
        }
        let Some(kind) = raw_string(envelope.kind) else {
            return;
        };
        match kind.as_str() {
            "session.start" => {
                let data: StartData<'_> = decode(envelope.data, &mut self.malformed_data);
                let session_id = raw_string(data.session_id);
                if self.start_seen {
                    if session_id.is_some() && session_id != self.start_session_id {
                        self.identity_conflict = true;
                    }
                } else {
                    self.start_seen = true;
                    self.start_session_id = session_id;
                    self.start_time = raw_string(data.start_time)
                        .as_deref()
                        .and_then(parse_rfc3339);
                    let context: ContextData<'_> = decode(data.context, &mut self.malformed_data);
                    self.cwd = raw_string(context.cwd);
                }
                if let Some(model) = raw_string(data.selected_model) {
                    self.observe_model(model, ts, true);
                }
                self.boot(ts);
                self.push_activity("session_start", ts);
            }
            "session.resume" => {
                self.boot(ts);
                self.push_activity("session_resume", ts);
            }
            "session.model_change" => {
                let data: ModelChangeData<'_> = decode(envelope.data, &mut self.malformed_data);
                if let Some(model) = raw_string(data.new_model) {
                    self.observe_model(model, ts, true);
                }
                self.push_activity("model_change", ts);
            }
            "assistant.turn_start" => {
                let data: TurnData<'_> = decode(envelope.data, &mut self.malformed_data);
                match raw_string(data.turn_id) {
                    Some(turn_id) => {
                        self.open_turns.insert(turn_id);
                    }
                    None => self.turn_ids_ambiguous = true,
                }
                self.lifecycle(ts);
                self.push_activity("turn_start", ts);
            }
            "assistant.turn_end" => {
                let data: TurnData<'_> = decode(envelope.data, &mut self.malformed_data);
                match raw_string(data.turn_id) {
                    Some(turn_id) => {
                        self.open_turns.remove(&turn_id);
                    }
                    None => self.turn_ids_ambiguous = true,
                }
                self.lifecycle(ts);
                self.push_activity("turn_end", ts);
            }
            "session.shutdown" => {
                let data: ShutdownData<'_> = decode(envelope.data, &mut self.malformed_data);
                if let Some(raw) = data.usage_accounting_watermarks {
                    match serde_json::from_str::<UsageWatermarks>(raw.get()) {
                        Ok(UsageWatermarks(watermarks)) => self.receipt_watermarks.push(watermarks),
                        Err(_) => self.receipt_ledger_invalid = true,
                    }
                } else if data.model_metrics.is_some() {
                    self.receipt_ledger_invalid = true;
                }
                if let Some(model) = raw_string(data.current_model) {
                    self.observe_model(model, ts, true);
                }
                self.shutdown_count += 1;
                // Without decodable metrics an older snapshot, if any, stays
                // superseded.
                if let Some(snapshot) = data
                    .model_metrics
                    .and_then(|raw| parse_model_metrics(raw, ts))
                {
                    for model in snapshot.models.clone() {
                        self.observe_model(model, ts, false);
                    }
                    self.usage = Some(snapshot);
                    self.usage_superseded = false;
                }
                self.open_turns.clear();
                self.turn_ids_ambiguous = false;
                self.lifecycle(ts);
                self.ended = true;
                self.push_activity("session_shutdown", ts);
            }
            "session.usage_record" => {
                self.usage_record_count += 1;
                let malformed_before = self.malformed_data;
                let data: UsageRecordData<'_> = decode(envelope.data, &mut self.malformed_data);
                if self.malformed_data != malformed_before {
                    self.receipt_accounting_seen = true;
                    self.receipt_ledger_invalid = true;
                }
                match data.usage.and_then(|raw| parse_usage_record(raw, ts)) {
                    Some(usage) => {
                        self.last_request = Some(usage);
                        self.last_request_unverified = false;
                    }
                    // An invalid newer record leaves the older valid one in
                    // place, flagged because a newer request may exist.
                    None => self.mark_last_request_unverified(),
                }
                if let Some(raw_usage) = data.usage {
                    let (accounting_present, receipt) = parse_usage_receipt(raw_usage);
                    self.receipt_accounting_seen |= accounting_present;
                    if let Some(receipt) = receipt {
                        self.add_receipt(receipt);
                        if let Some(ts) = ts {
                            self.receipt_last_at =
                                Some(self.receipt_last_at.map_or(ts, |last| last.max(ts)));
                        }
                    } else if accounting_present {
                        self.receipt_ledger_invalid = true;
                    } else {
                        self.receipt_events_without_accounting += 1;
                    }
                } else {
                    self.receipt_accounting_seen = true;
                    self.receipt_ledger_invalid = true;
                }
            }
            kind if kind.contains("checkpoint") => {
                self.receipt_accounting_seen = true;
                self.receipt_ledger_invalid = true;
            }
            _ => {}
        }
    }

    fn add_receipt(&mut self, receipt: UsageReceipt) {
        let sequence_key = (receipt.source.clone(), receipt.sequence);
        if self
            .receipt_by_sequence
            .get(&sequence_key)
            .is_some_and(|existing| existing != &receipt)
            || self
                .receipt_by_id
                .get(&receipt.usage_id)
                .is_some_and(|existing| existing != &receipt)
        {
            self.receipt_ledger_invalid = true;
            return;
        }
        self.receipt_by_sequence
            .entry(sequence_key)
            .or_insert_with(|| receipt.clone());
        self.receipt_by_id
            .entry(receipt.usage_id.clone())
            .or_insert(receipt);
    }

    fn verified_receipt_total(&self) -> Option<(u64, u64)> {
        if !self.receipt_accounting_seen
            || !self.start_seen
            || self.start_session_id.is_none()
            || self.receipt_ledger_invalid
            || self.receipt_by_sequence.is_empty()
            || self.receipt_by_sequence.len() != self.receipt_by_id.len()
            || self.receipt_events_without_accounting > 0
        {
            return None;
        }
        let mut next = HashMap::<&str, u64>::new();
        let mut input = 0_u64;
        let mut output = 0_u64;
        for receipt in self.receipt_by_sequence.values() {
            let expected = next.entry(&receipt.source).or_insert(1);
            if receipt.sequence != *expected {
                return None;
            }
            *expected = expected.checked_add(1)?;
            input = input.checked_add(receipt.input)?;
            output = output.checked_add(receipt.output)?;
        }
        let mut previous_watermarks = HashMap::<&str, u64>::new();
        for watermarks in &self.receipt_watermarks {
            for (source, watermark) in watermarks {
                let covered = next.get(source.as_str()).copied().unwrap_or(1) - 1;
                if *watermark > covered
                    || previous_watermarks
                        .get(source.as_str())
                        .is_some_and(|previous| watermark < previous)
                {
                    return None;
                }
                previous_watermarks.insert(source, *watermark);
            }
        }
        Some((input, output))
    }

    /// A new client process starts with no turns in flight.
    fn boot(&mut self, ts: Option<DateTime<Utc>>) {
        self.open_turns.clear();
        self.turn_ids_ambiguous = false;
        self.lifecycle(ts);
    }

    fn lifecycle(&mut self, ts: Option<DateTime<Utc>>) {
        self.unreadable_since_lifecycle = false;
        if ts.is_some() {
            self.lifecycle_at = ts;
        }
    }

    fn observe_model(&mut self, name: String, ts: Option<DateTime<Utc>>, selected: bool) {
        let Some(name) = clean_model_name(name) else {
            return;
        };
        if !self.models.contains(&name) && self.models.len() < MAX_MODELS {
            self.models.push(name.clone());
        }
        if selected {
            self.model = Some(name);
            self.model_at = ts;
        }
    }

    fn push_activity(&mut self, kind: &str, ts: Option<DateTime<Utc>>) {
        if self.activity.len() == MAX_ACTIVITY {
            self.activity.pop_front();
        }
        self.activity.push_back(RolloutActivity {
            kind: kind.to_string(),
            tool_name: None,
            status: None,
            ts,
        });
    }

    fn finish(
        self,
        session_id: &str,
        path: &Path,
        warnings: &mut Vec<String>,
    ) -> Option<CopilotThreadRecord> {
        let mismatched = self
            .start_session_id
            .as_deref()
            .is_some_and(|id| id != session_id);
        if self.identity_conflict || mismatched {
            warnings.push(format!(
                "Copilot session identity does not match its directory; skipped: {}",
                path.display()
            ));
            return None;
        }
        if self.malformed > 0 {
            warnings.push(format!(
                "Skipped {} malformed Copilot event lines: {}",
                self.malformed,
                path.display()
            ));
        }
        if self.malformed_data > 0 {
            warnings.push(format!(
                "Ignored unsupported data in {} Copilot events: {}",
                self.malformed_data,
                path.display()
            ));
        }
        if self.oversized > 0 {
            warnings.push(format!(
                "Skipped {} Copilot event lines over {MAX_LINE_BYTES} bytes: {}",
                self.oversized,
                path.display()
            ));
        }
        if self.truncated_tail {
            warnings.push(format!(
                "Copilot event log tail appears truncated: {}",
                path.display()
            ));
        }

        let tail_uncertain =
            self.truncated_tail || self.incomplete || self.unreadable_since_lifecycle;
        let (state, state_detail) = if tail_uncertain {
            (
                ThreadState::Unknown,
                "the latest persisted records are unreadable or truncated, so the lifecycle is not provable",
            )
        } else if self.ended {
            (
                ThreadState::Idle,
                "session.shutdown is the latest persisted event",
            )
        } else if self.turn_ids_ambiguous {
            (
                ThreadState::Unknown,
                "assistant turn events without a turnId cannot be paired, so the lifecycle is not provable",
            )
        } else if !self.open_turns.is_empty()
            && !self.turn_ids_ambiguous
            && copilot_turn_lock_is_live(path.parent().unwrap_or(Path::new("")))
        {
            (
                ThreadState::Running,
                "an unmatched assistant turn is backed by a live Copilot session lock",
            )
        } else if !self.open_turns.is_empty() {
            (
                ThreadState::Unknown,
                "assistant.turn_start has no matching assistant.turn_end, but persisted events alone do not prove the client is still running",
            )
        } else if self.records == 0 {
            (ThreadState::Unknown, "no readable Copilot events")
        } else {
            (
                ThreadState::Unknown,
                "no assistant turn is open and no session.shutdown follows; session.idle is not persisted, so idle is not provable",
            )
        };

        let receipt_total = self.verified_receipt_total();
        let receipt_evidence = self.receipt_accounting_seen;
        let token_usage = if let Some((input, output)) = receipt_total {
            Some(CopilotTokenUsage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
                models: self.models.clone(),
                observed_at: self.receipt_last_at,
                status: UsageSnapshotStatus::VerifiedReceipts,
                shutdown_count: self.shutdown_count,
            })
        } else {
            self.usage.map(|usage| CopilotTokenUsage {
                input_tokens: usage.input,
                output_tokens: usage.output,
                cache_read_tokens: usage.cache_read,
                cache_write_tokens: usage.cache_write,
                reasoning_tokens: usage.reasoning,
                models: usage.models,
                observed_at: usage.observed_at,
                status: if receipt_evidence || tail_uncertain {
                    UsageSnapshotStatus::Uncertain
                } else if self.usage_superseded {
                    UsageSnapshotStatus::Superseded
                } else {
                    UsageSnapshotStatus::Latest
                },
                shutdown_count: self.shutdown_count,
            })
        };
        let last_request_unverified = self.last_request_unverified || self.incomplete;
        let last_request_usage = self.last_request.map(|usage| CopilotLastRequestUsage {
            followed_by_unverified_records: last_request_unverified,
            ..usage
        });

        Some(CopilotThreadRecord {
            id: format!("{ID_PREFIX}{session_id}"),
            session_id: session_id.to_string(),
            created_at: self.start_time.or(self.first_at),
            updated_at: self.last_at,
            recency_at: self.last_at,
            cwd: self.cwd,
            source_kind: SOURCE_KIND.to_string(),
            model: self.model,
            model_observed_at: self.model_at,
            models: self.models,
            state,
            state_detail: state_detail.to_string(),
            lifecycle_at: self.lifecycle_at,
            activity: self.activity.into_iter().collect(),
            token_usage,
            last_request_usage,
            path: path.to_path_buf(),
        })
    }
}

fn copilot_turn_lock_is_live(session_dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(session_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid_text) = name
            .strip_prefix("inuse.")
            .and_then(|name| name.strip_suffix(".lock"))
        else {
            continue;
        };
        if pid_text.is_empty() || !pid_text.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = pid_text.parse::<u32>() else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.len() > 32 {
            continue;
        }
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let Ok(opened_metadata) = file.metadata() else {
            continue;
        };
        if !opened_metadata.is_file() {
            continue;
        }
        let mut content = String::new();
        if BufReader::new(file)
            .take(33)
            .read_to_string(&mut content)
            .is_err()
            || content.len() > 32
            || content.trim().parse::<u32>().ok() != Some(pid)
        {
            continue;
        }
        if process_is_alive(pid) {
            return true;
        }
    }
    false
}

/// Decodes the five known counters of one `session.usage_record` `usage`
/// object. Returns `None` when the object is not decodable, when any present
/// counter is not a non-negative integer that fits in `u64`, or when no
/// counter is present. Absent counters stay `None` and are never zero-filled.
fn parse_usage_record(
    raw: &RawValue,
    observed_at: Option<DateTime<Utc>>,
) -> Option<CopilotLastRequestUsage> {
    let usage = serde_json::from_str::<UsageData<'_>>(raw.get()).ok()?;
    let fields = [
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.reasoning_tokens,
    ];
    let mut values = [None; 5];
    for (slot, field) in values.iter_mut().zip(fields) {
        if let Some(field) = field {
            *slot = Some(serde_json::from_str::<u64>(field.get()).ok()?);
        }
    }
    if values.iter().all(Option::is_none) {
        return None;
    }
    let [input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens] =
        values;
    Some(CopilotLastRequestUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        observed_at,
        followed_by_unverified_records: false,
    })
}

fn parse_usage_receipt(raw: &RawValue) -> (bool, Option<UsageReceipt>) {
    let Ok(usage_data) = serde_json::from_str::<UsageData<'_>>(raw.get()) else {
        return (false, None);
    };
    let Some(raw_accounting) = usage_data.accounting else {
        return (false, None);
    };
    let Some(accounting) = serde_json::from_str::<UsageAccounting>(raw_accounting.get()).ok()
    else {
        return (true, None);
    };
    let (Some(source), Some(sequence), Some(usage_id)) = (
        accounting.source_session_id,
        accounting.sequence,
        accounting.usage_id,
    ) else {
        return (true, None);
    };
    if source.trim().is_empty()
        || source.len() > MAX_SESSION_ID_LEN
        || sequence == 0
        || usage_id.trim().is_empty()
        || usage_id.len() > 256
    {
        return (true, None);
    }
    let Some(usage) = parse_usage_record(raw, None) else {
        return (true, None);
    };
    let Some(input) = usage.input_tokens else {
        return (true, None);
    };
    let Some(output) = usage.output_tokens else {
        return (true, None);
    };
    (
        true,
        Some(UsageReceipt {
            source,
            sequence,
            usage_id,
            input,
            output,
        }),
    )
}

/// Sums `modelMetrics.<model>.usage` counters across models. Returns `None`
/// when any model entry lacks a decodable usage object, because the sum would
/// then undercount.
fn parse_model_metrics(
    raw: &RawValue,
    observed_at: Option<DateTime<Utc>>,
) -> Option<UsageSnapshot> {
    let metrics = serde_json::from_str::<BTreeMap<String, &RawValue>>(raw.get()).ok()?;
    if metrics.is_empty() {
        return None;
    }
    let mut sums = [FieldSum::default(); 5];
    let mut models = Vec::new();
    for (name, metric) in &metrics {
        let metric = serde_json::from_str::<MetricData<'_>>(metric.get()).ok()?;
        let usage = serde_json::from_str::<UsageData<'_>>(metric.usage?.get()).ok()?;
        let values = [
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            usage.reasoning_tokens,
        ];
        for (sum, value) in sums.iter_mut().zip(values) {
            sum.add(raw_u64(value));
        }
        if let Some(name) = clean_model_name(name.clone()) {
            models.push(name);
        }
    }
    let entries = metrics.len();
    let [input, output, cache_read, cache_write, reasoning] = sums.map(|sum| sum.value(entries));
    if [input, output, cache_read, cache_write, reasoning]
        .iter()
        .all(Option::is_none)
    {
        return None;
    }
    Some(UsageSnapshot {
        input,
        output,
        cache_read,
        cache_write,
        reasoning,
        models,
        observed_at,
    })
}

#[derive(Debug, Default, Clone, Copy)]
struct FieldSum {
    total: u64,
    seen: usize,
    overflow: bool,
}

impl FieldSum {
    fn add(&mut self, value: Option<u64>) {
        let Some(value) = value else {
            return;
        };
        self.seen += 1;
        match self.total.checked_add(value) {
            Some(total) => self.total = total,
            None => self.overflow = true,
        }
    }

    fn value(self, entries: usize) -> Option<u64> {
        (self.seen == entries && !self.overflow).then_some(self.total)
    }
}

fn decode<'a, T: Deserialize<'a> + Default>(
    data: Option<&'a RawValue>,
    malformed: &mut usize,
) -> T {
    match data {
        None => T::default(),
        Some(raw) => serde_json::from_str::<T>(raw.get()).unwrap_or_else(|_| {
            *malformed += 1;
            T::default()
        }),
    }
}

fn raw_string(raw: Option<&RawValue>) -> Option<String> {
    raw.and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
        .filter(|text| !text.is_empty())
}

fn raw_u64(raw: Option<&RawValue>) -> Option<u64> {
    raw.and_then(|raw| serde_json::from_str::<u64>(raw.get()).ok())
}

fn clean_model_name(name: String) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_MODEL_NAME_LEN || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.to_string())
}

fn parse_rfc3339(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= MAX_SESSION_ID_LEN
        && session_id != "."
        && session_id != ".."
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn bounded_reader_discards_oversized_lines_and_reports_unterminated_tail() {
        let input = b"short\nthis line is far too long\nok\npartial".to_vec();
        let mut reader = BufReader::with_capacity(4, input.as_slice());
        let mut buf = Vec::new();
        let mut seen = Vec::new();
        loop {
            buf.clear();
            match read_bounded_line(&mut reader, &mut buf, 10).unwrap() {
                LineRead::Eof => break,
                LineRead::Oversized => seen.push("oversized".to_string()),
                LineRead::Line { terminated } => seen.push(format!(
                    "{}:{terminated}",
                    String::from_utf8_lossy(&buf).trim_end()
                )),
            }
        }
        assert_eq!(
            seen,
            vec!["short:true", "oversized", "ok:true", "partial:false"]
        );
    }

    #[test]
    fn metrics_require_every_model_to_report_a_counter() {
        let raw = RawValue::from_string(
            serde_json::json!({
                "a": { "usage": { "inputTokens": 5, "outputTokens": 1, "reasoningTokens": 2 } },
                "b": { "usage": { "inputTokens": 7, "outputTokens": 3 } }
            })
            .to_string(),
        )
        .unwrap();
        let usage = parse_model_metrics(&raw, None).unwrap();
        assert_eq!(usage.input, Some(12));
        assert_eq!(usage.output, Some(4));
        assert_eq!(usage.reasoning, None);
        assert_eq!(usage.models, vec!["a", "b"]);

        let missing = RawValue::from_string(
            serde_json::json!({
                "a": { "usage": { "inputTokens": 5 } },
                "b": { "requests": { "count": 1 } }
            })
            .to_string(),
        )
        .unwrap();
        assert!(parse_model_metrics(&missing, None).is_none());
    }

    #[test]
    fn vscode_replays_allowlisted_copilot_metadata_without_retaining_transcript_content() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("chat.jsonl");
        let fixture = [
            r#"{"kind":0,"v":{"version":3,"sessionId":"vscode-1","requests":[]}}"#,
            r#"{"kind":2,"k":["requests"],"v":[{"requestId":"r1","agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/example-model","prompt":"PRIVATE_PROMPT"}]}"#,
            r#"{"kind":1,"k":["requests",0,"modelId"],"v":"copilot/next-model"}"#,
            r#"{"kind":1,"k":["requests",0,"promptTokens"],"v":12}"#,
        ].join("\n");
        fs::write(&path, fixture).unwrap();
        let record = read_vscode_file(&path, false, Utc::now()).unwrap();
        assert!(record.id.starts_with("copilot-vscode:"), "{}", record.id);
        assert!(record.id.ends_with(":vscode-1"), "{}", record.id);
        assert_eq!(record.model.as_deref(), Some("copilot/next-model"));
        assert_eq!(record.models, vec!["copilot/next-model"]);
        assert_eq!(record.state, ThreadState::Unknown);
        assert!(record.token_usage.is_none());
        assert!(!format!("{record:?}").contains("PRIVATE_PROMPT"));
    }

    #[test]
    fn vscode_snapshot_projects_metadata_without_retaining_prompt_or_response_content() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("snapshot.json");
        fs::write(&path, r#"{"version":3,"sessionId":"snapshot","requests":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/model","prompt":"PRIVATE_PROMPT","response":{"text":"PRIVATE_RESPONSE"}}]}"#).unwrap();
        let record = read_vscode_file(&path, true, Utc::now()).unwrap();
        assert_eq!(record.model.as_deref(), Some("copilot/model"));
        assert!(!format!("{record:?}").contains("PRIVATE_PROMPT"));
        assert!(!format!("{record:?}").contains("PRIVATE_RESPONSE"));
    }

    #[test]
    fn vscode_does_not_claim_sessions_without_explicit_copilot_extension_identity() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("chat.json");
        fs::write(
            &path,
            r#"{"sessionId":"other","requests":[{"modelId":"model-a","prompt":"PRIVATE"}]}"#,
        )
        .unwrap();
        assert!(read_vscode_file(&path, true, Utc::now()).is_none());
    }

    #[test]
    fn vscode_keeps_confirmed_session_when_model_is_unknown() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("chat.json");
        fs::write(&path, r#"{"sessionId":"unknown-model","requests":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}}}]}"#).unwrap();
        let record = read_vscode_file(&path, true, Utc::now()).unwrap();
        assert!(record.model.is_none());
        assert!(record.models.is_empty());
        assert_eq!(record.state, ThreadState::Unknown);
        assert!(record.token_usage.is_none());
    }

    #[test]
    fn vscode_replay_preserves_indexes_and_accepts_truncate_without_append_value() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("workspace").join("chatSessions");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("indexed.jsonl");
        let log = [
            r#"{"kind":0,"v":{"sessionId":"indexed","requests":[{"modelId":"other"},{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/kept"}]}}"#,
            r#"{"kind":3,"k":["requests",0]}"#,
            r#"{"kind":1,"k":["requests",1,"modelId"],"v":"copilot/updated"}"#,
            r#"{"kind":2,"k":["requests"],"i":2}"#,
        ].join("\n");
        fs::write(&path, log).unwrap();
        let record = read_vscode_file(&path, false, Utc::now()).unwrap();
        assert_eq!(record.model.as_deref(), Some("copilot/updated"));
    }

    #[test]
    fn vscode_rejects_missing_initial_and_out_of_range_mutations() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("bad.jsonl");
        fs::write(&path, r#"{"kind":2,"k":["requests"],"v":[]}"#).unwrap();
        assert!(read_vscode_file(&path, false, Utc::now()).is_none());
        fs::write(&path, concat!(
            r#"{"kind":0,"v":{"sessionId":"truncated","requests":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/model"}]}}"#,
            "\n",
            r#"{"kind":2,"k":["requests"],"i":0}"#
        )).unwrap();
        assert!(read_vscode_file(&path, false, Utc::now()).is_none());
        fs::write(&path, concat!(
            r#"{"kind":0,"v":{"sessionId":"bad","requests":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/model"}]}}"#,
            "\n",
            r#"{"kind":1,"k":["requests",9,"modelId"],"v":"copilot/other"}"#
        )).unwrap();
        assert!(read_vscode_file(&path, false, Utc::now()).is_none());
    }

    #[test]
    fn vscode_prefers_jsonl_when_snapshot_and_operation_log_share_session_name() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("workspace").join("chatSessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join("same.json"), r#"{"sessionId":"same","requests":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/snapshot"}]}"#).unwrap();
        fs::write(sessions.join("same.jsonl"), concat!(
            r#"{"kind":0,"v":{"sessionId":"same","requests":[]}}"#,
            "\n",
            r#"{"kind":2,"k":["requests"],"v":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/log"}]}"#
        )).unwrap();
        let records = read_vscode_workspace(&sessions, Utc::now(), &mut Vec::new());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model.as_deref(), Some("copilot/log"));
    }

    #[test]
    fn vscode_workspace_reports_safe_aggregate_for_malformed_and_oversized_files() {
        let temp = TempDir::new().unwrap();
        let sessions = temp.path().join("workspace").join("chatSessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("sensitive-name.json"),
            "PRIVATE_CORRUPT_CONTENT",
        )
        .unwrap();
        let oversized = fs::File::create(sessions.join("large.json")).unwrap();
        oversized.set_len(16 * 1024 * 1024 + 1).unwrap();
        let mut warnings = Vec::new();
        assert!(read_vscode_workspace(&sessions, Utc::now(), &mut warnings).is_empty());
        assert!(warnings[0].contains("1 over size limits"));
        assert!(warnings[0].contains("1 malformed"));
        assert!(!warnings[0].contains("sensitive-name"));
        assert!(!warnings[0].contains("PRIVATE_CORRUPT_CONTENT"));
    }

    #[test]
    fn vscode_replays_whole_request_array_replacement() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("replace.jsonl");
        fs::write(&path, concat!(
            r#"{"kind":0,"v":{"sessionId":"replace","requests":[]}}"#,
            "\n",
            r#"{"kind":1,"k":["requests"],"v":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/replaced"}]}"#
        )).unwrap();
        let record = read_vscode_file(&path, false, Utc::now()).unwrap();
        assert_eq!(record.model.as_deref(), Some("copilot/replaced"));
    }

    #[test]
    fn vscode_null_mutations_clear_then_allow_safe_agent_and_model_metadata() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nullable.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"kind":0,"v":{"sessionId":"nullable","requests":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/old"}]}}"#,
                "\n",
                r#"{"kind":1,"k":["requests",0,"modelId"],"v":null}"#,
                "\n",
                r#"{"kind":1,"k":["requests",0,"modelId"],"v":"copilot/model-final"}"#,
                "\n",
                r#"{"kind":1,"k":["requests",0,"agent"],"v":null}"#,
                "\n",
                r#"{"kind":1,"k":["requests",0,"agent"],"v":{"extensionId":{"value":"GitHub.copilot-chat"}}}"#,
                "\n",
                r#"{"kind":1,"k":["requests",0,"agent","extensionId"],"v":null}"#,
                "\n",
                r#"{"kind":1,"k":["requests",0,"agent","extensionId"],"v":{"value":"GitHub.copilot-chat"}}"#,
                "\n",
                r#"{"kind":1,"k":["requests",0,"agent","extensionId","value"],"v":null}"#,
                "\n",
                r#"{"kind":1,"k":["requests",0,"agent","extensionId","value"],"v":"GitHub.copilot-chat"}"#
            ),
        )
        .unwrap();
        let record = read_vscode_file_checked(&path, false, Utc::now())
            .unwrap_or_else(|error| panic!("{error:?}"))
            .unwrap();
        assert_eq!(record.model.as_deref(), Some("copilot/model-final"));
    }

    #[test]
    fn vscode_ignores_valid_nested_response_append_without_treating_session_as_malformed() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nested.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"kind":0,"v":{"sessionId":"nested","requests":[{"agent":{"extensionId":{"value":"GitHub.copilot-chat"}},"modelId":"copilot/model"}]}}"#,
                "\n",
                r#"{"kind":2,"k":["requests",0,"response"],"v":[{"text":"PRIVATE_RESPONSE"}]}"#
            ),
        )
        .unwrap();
        let record = read_vscode_file(&path, false, Utc::now()).unwrap();
        assert_eq!(record.model.as_deref(), Some("copilot/model"));
        assert!(!format!("{record:?}").contains("PRIVATE_RESPONSE"));
    }
}
