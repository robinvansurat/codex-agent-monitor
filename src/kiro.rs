//! Read-only ingestion for the local Kiro CLI session formats.
//!
//! This module deliberately parses only telemetry needed by the monitor.  It
//! never retains titles, prompts, message bodies, arguments, or tool results.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::model::{ContextUsage, TaskProgress, TaskStatus, ThreadState, ThreadTask};
use crate::rollout::{RolloutActivity, RolloutTerminalEvent};

#[derive(Debug, Clone)]
pub struct KiroThreadRecord {
    pub id: String,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub recency_at: Option<DateTime<Utc>>,
    pub cwd: Option<String>,
    pub nickname: Option<String>,
    pub role: Option<String>,
    pub source_kind: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub state: ThreadState,
    pub lifecycle_at: Option<DateTime<Utc>>,
    pub terminal: Option<(RolloutTerminalEvent, Option<DateTime<Utc>>)>,
    pub activity: Vec<RolloutActivity>,
    pub context_usage: Option<ContextUsage>,
    pub context_usage_at: Option<DateTime<Utc>>,
    pub task_progress: Option<TaskProgress>,
    pub task_progress_at: Option<DateTime<Utc>>,
    pub task_progress_detail: Option<String>,
    pub path: Option<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
pub struct KiroSnapshot {
    pub threads: Vec<KiroThreadRecord>,
    pub warnings: Vec<String>,
}

type TerminalObservation = Option<(RolloutTerminalEvent, Option<DateTime<Utc>>)>;
type NativeEventParse = (
    Vec<RolloutActivity>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);
type AcpMessageParse = (
    Vec<RolloutActivity>,
    TerminalObservation,
    ThreadState,
    Option<DateTime<Utc>>,
);
type ClassicHistoryParse = (
    Vec<RolloutActivity>,
    TerminalObservation,
    Option<DateTime<Utc>>,
);

pub fn resolve_kiro_home(cli_home: Option<&str>) -> Result<(PathBuf, bool)> {
    if let Some(home) = cli_home {
        return Ok((PathBuf::from(home), true));
    }
    if let Some(home) = std::env::var_os("KIRO_HOME") {
        return Ok((PathBuf::from(home), true));
    }
    let home = dirs::home_dir()
        .map(|path| path.join(".kiro"))
        .ok_or_else(|| anyhow::anyhow!("Unable to resolve home dir for default KIRO_HOME"))?;
    Ok((home, false))
}

pub fn read_snapshot(
    home: &Path,
    db_override: Option<&Path>,
    home_was_explicit: bool,
) -> KiroSnapshot {
    let mut snapshot = KiroSnapshot::default();
    let mut by_id = HashMap::<String, KiroThreadRecord>::new();

    let native = read_native(home);
    snapshot.warnings.extend(native.warnings);
    for thread in native.threads {
        by_id.insert(thread.id.clone(), thread);
    }

    let acp = read_acp(home);
    snapshot.warnings.extend(acp.warnings);
    for thread in acp.threads {
        by_id.entry(thread.id.clone()).or_insert(thread);
    }

    let db_path = db_override.map(Path::to_path_buf).or_else(|| {
        if home_was_explicit {
            Some(home.join("data.sqlite3"))
        } else {
            default_classic_db()
        }
    });
    if let Some(db_path) = db_path {
        let classic = read_classic_db(&db_path);
        snapshot.warnings.extend(classic.warnings);
        for thread in classic.threads {
            // Native session files are the authoritative representation when
            // the same conversation id is present in the legacy database.
            by_id.entry(thread.id.clone()).or_insert(thread);
        }
    }

    snapshot.threads = by_id.into_values().collect();
    snapshot.threads.sort_by(|a, b| {
        b.recency_at
            .cmp(&a.recency_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    snapshot
}

fn native_context_usage(
    state: &Value,
    path: &Path,
    warnings: &mut Vec<String>,
) -> Option<ContextUsage> {
    let rts = state.get("rts_model_state").unwrap_or(&Value::Null);
    let conversation = state.get("conversation_metadata").unwrap_or(&Value::Null);
    let percentage_values = [
        rts.get("context_usage_percentage"),
        conversation
            .get("last_context_usage")
            .and_then(|value| value.get("percentage")),
    ];
    let saw_percentage = percentage_values
        .iter()
        .flatten()
        .any(|value| !value.is_null());
    let used_percent = percentage_values
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .find(|value| value.is_finite() && (0.0..=100.0).contains(value));
    let context_window_tokens = rts
        .get("model_info")
        .and_then(|value| value.get("context_window_tokens"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0);

    let Some(used_percent) = used_percent else {
        if saw_percentage {
            warnings.push(format!(
                "malformed Kiro native context usage: {}",
                path.display()
            ));
        }
        return None;
    };
    let Some(context_window_tokens) = context_window_tokens else {
        warnings.push(format!(
            "incomplete Kiro native context usage: {}",
            path.display()
        ));
        return None;
    };
    let used_tokens_approx = ((used_percent / 100.0) * context_window_tokens as f64).round() as u64;
    Some(ContextUsage {
        used_percent,
        used_tokens_approx: used_tokens_approx.min(context_window_tokens),
        context_window_tokens,
    })
}

fn read_native(home: &Path) -> KiroSnapshot {
    let mut out = KiroSnapshot::default();
    let dir = home.join("sessions").join("cli");
    let entries = match immediate_entries(&dir, &mut out.warnings) {
        Some(entries) => entries,
        None => return out,
    };
    for path in entries {
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let Some(root) = read_json(&path, &mut out.warnings) else {
            continue;
        };
        let Some(id) = string_at(&root, &["session_id"]).filter(|id| !id.is_empty()) else {
            out.warnings
                .push(format!("Kiro native session has no id: {}", path.display()));
            continue;
        };
        let Some(state) = root.get("session_state").filter(|value| value.is_object()) else {
            out.warnings.push(format!(
                "unsupported Kiro native session schema: {}",
                path.display()
            ));
            continue;
        };
        let rts = state.get("rts_model_state").unwrap_or(&Value::Null);
        let model = string_at(rts, &["model_info", "model_id"]);
        let effort = string_at(
            rts,
            &["additional_fields", "overrides", "output_config", "effort"],
        );
        let created_at = string_at(&root, &["created_at"])
            .as_deref()
            .and_then(parse_rfc3339);
        let updated_at = string_at(&root, &["updated_at"])
            .as_deref()
            .and_then(parse_rfc3339);
        let context_usage = native_context_usage(state, &path, &mut out.warnings);
        let cwd = string_at(&root, &["cwd"]);
        let role = string_at(state, &["agent_name"]);
        let metadata = state
            .get("conversation_metadata")
            .and_then(|v| v.get("user_turn_metadatas"));
        let mut terminal: Option<(RolloutTerminalEvent, Option<DateTime<Utc>>)> = None;
        if let Some(items) = metadata.and_then(Value::as_array) {
            for item in items {
                if item.get("end_reason").and_then(Value::as_str) != Some("UserTurnEnd") {
                    continue;
                }
                let Some(ts) = item
                    .get("end_timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339)
                else {
                    continue;
                };
                let event = if item
                    .get("result")
                    .and_then(Value::as_object)
                    .is_some_and(|v| v.contains_key("Err"))
                {
                    RolloutTerminalEvent::Failed
                } else if item
                    .get("result")
                    .and_then(Value::as_object)
                    .is_some_and(|v| v.contains_key("Ok"))
                {
                    RolloutTerminalEvent::Completed
                } else {
                    continue;
                };
                if terminal
                    .as_ref()
                    .is_none_or(|(_, old)| old.is_none_or(|old| ts > old))
                {
                    terminal = Some((event, Some(ts)));
                }
            }
        }
        let jsonl = path.with_extension("jsonl");
        let (activities, latest_prompt, latest_event) =
            read_native_events(&jsonl, &mut out.warnings);
        let tasks_dir = path.with_extension("").join("tasks");
        let (task_progress, task_progress_at) = read_native_tasks(&tasks_dir, &mut out.warnings);
        let pending_prompt = latest_prompt.filter(|prompt| {
            terminal
                .as_ref()
                .and_then(|(_, timestamp)| *timestamp)
                .is_none_or(|end| *prompt > end)
        });
        let state = if pending_prompt.is_some()
            && native_session_lock_is_live(&path.with_extension("lock"), &mut out.warnings)
        {
            ThreadState::Running
        } else if pending_prompt.is_some() {
            ThreadState::Unknown
        } else {
            terminal
                .as_ref()
                .map(|(event, _)| match event {
                    RolloutTerminalEvent::Completed | RolloutTerminalEvent::Failed => {
                        ThreadState::Idle
                    }
                    RolloutTerminalEvent::Interrupted => ThreadState::Unknown,
                })
                .unwrap_or(ThreadState::Unknown)
        };
        let recency_at = [
            updated_at,
            latest_event,
            terminal.as_ref().and_then(|(_, ts)| *ts),
            task_progress_at,
        ]
        .into_iter()
        .flatten()
        .max();
        out.threads.push(KiroThreadRecord {
            id: format!("kiro:{id}"),
            created_at,
            updated_at,
            recency_at,
            cwd,
            nickname: None,
            role,
            source_kind: "kiro_cli".to_string(),
            model,
            effort,
            state,
            lifecycle_at: [latest_prompt, terminal.as_ref().and_then(|(_, ts)| *ts)]
                .into_iter()
                .flatten()
                .max(),
            terminal,
            activity: activities,
            context_usage,
            context_usage_at: updated_at,
            task_progress_detail: task_progress
                .is_none()
                .then(|| "Kiro did not persist a task plan".to_string()),
            task_progress,
            task_progress_at,
            path: Some(path),
            warnings: Vec::new(),
        });
    }
    out
}

fn native_session_lock_is_live(path: &Path, warnings: &mut Vec<String>) -> bool {
    if !path.exists() {
        return false;
    }
    if is_symlink_or_parent(path) {
        warnings.push(format!(
            "skipping symlinked Kiro native session lock: {}",
            path.display()
        ));
        return false;
    }
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(err) => {
            warnings.push(format!(
                "unable to read Kiro native session lock {}: {err}",
                path.display()
            ));
            return false;
        }
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        warnings.push(format!(
            "malformed Kiro native session lock: {}",
            path.display()
        ));
        return false;
    };
    let valid_started_at = value
        .get("started_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .is_some();
    let pid = value
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    if !valid_started_at || pid.is_none() {
        warnings.push(format!(
            "unsupported Kiro native session lock schema: {}",
            path.display()
        ));
        return false;
    }
    process_is_alive(pid.unwrap_or_default())
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 does not modify the target process; it only checks PID visibility.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the handle is checked for null, queried without mutation, and always closed.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0_u32;
    // SAFETY: exit_code is a valid writable pointer for this call.
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    // SAFETY: handle was returned by OpenProcess and has not been closed yet.
    unsafe {
        CloseHandle(handle);
    }
    queried && exit_code == STILL_ACTIVE as u32
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

fn read_native_events(path: &Path, warnings: &mut Vec<String>) -> NativeEventParse {
    if is_symlink_or_parent(path) {
        warnings.push(format!(
            "skipping symlinked Kiro native event log: {}",
            path.display()
        ));
        return (Vec::new(), None, None);
    }
    let Ok(raw) = fs::read_to_string(path) else {
        return (Vec::new(), None, None);
    };
    let mut activities = Vec::new();
    let mut tool_names = HashMap::<String, String>::new();
    let mut latest_prompt = None;
    let mut latest_event = None;
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            warnings.push(format!("malformed Kiro native event in {}", path.display()));
            continue;
        };
        let Some(kind) = value.get("kind").and_then(Value::as_str) else {
            continue;
        };
        let data = value.get("data").unwrap_or(&Value::Null);
        let timestamp = data
            .get("meta")
            .and_then(|v| v.get("timestamp"))
            .and_then(parse_unix_timestamp);
        latest_event = max_time(latest_event, timestamp);
        if kind == "Prompt" {
            latest_prompt = max_time(latest_prompt, timestamp);
            activities.push(RolloutActivity {
                kind: "prompt".to_string(),
                tool_name: None,
                status: None,
                ts: timestamp,
            });
            continue;
        }

        let content = data
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut emitted_detail = false;
        if kind == "AssistantMessage" {
            for item in content {
                if item.get("kind").and_then(Value::as_str) != Some("toolUse") {
                    continue;
                }
                let detail = item.get("data").unwrap_or(&Value::Null);
                let tool_name = string_at(detail, &["name"]);
                if let (Some(tool_id), Some(tool_name)) =
                    (string_at(detail, &["toolUseId"]), tool_name.clone())
                {
                    tool_names.insert(tool_id, tool_name);
                }
                activities.push(RolloutActivity {
                    kind: "tool_call".to_string(),
                    tool_name,
                    status: None,
                    ts: timestamp,
                });
                emitted_detail = true;
            }
            if !emitted_detail {
                activities.push(RolloutActivity {
                    kind: "assistant".to_string(),
                    tool_name: None,
                    status: None,
                    ts: timestamp,
                });
            }
            continue;
        }
        if kind == "ToolResults" {
            for item in content {
                if item.get("kind").and_then(Value::as_str) != Some("toolResult") {
                    continue;
                }
                let detail = item.get("data").unwrap_or(&Value::Null);
                let tool_name = string_at(detail, &["toolUseId"])
                    .and_then(|tool_id| tool_names.get(&tool_id).cloned());
                activities.push(RolloutActivity {
                    kind: "tool_result".to_string(),
                    tool_name,
                    status: string_at(detail, &["status"]),
                    ts: timestamp,
                });
                emitted_detail = true;
            }
            if !emitted_detail {
                activities.push(RolloutActivity {
                    kind: "tool_results".to_string(),
                    tool_name: None,
                    status: None,
                    ts: timestamp,
                });
            }
            continue;
        }

        activities.push(RolloutActivity {
            kind: kind.to_ascii_lowercase(),
            tool_name: None,
            status: None,
            ts: timestamp,
        });
    }
    (activities, latest_prompt, latest_event)
}

const MAX_KIRO_TASKS: usize = 1_000;

fn read_native_tasks(
    path: &Path,
    warnings: &mut Vec<String>,
) -> (Option<TaskProgress>, Option<DateTime<Utc>>) {
    let Some(entries) = immediate_entries(path, warnings) else {
        return (None, None);
    };
    if entries.len() > MAX_KIRO_TASKS {
        warnings.push(format!(
            "Kiro task store exceeds {MAX_KIRO_TASKS} entries: {}",
            path.display()
        ));
    }

    let mut tasks = Vec::new();
    let mut observed_at = None;
    for task_path in entries.into_iter().take(MAX_KIRO_TASKS) {
        if task_path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(file_id) = task_path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        let raw = match fs::read_to_string(&task_path) {
            Ok(raw) => raw,
            Err(err) => {
                warnings.push(format!("unable to read Kiro task #{file_id}: {err}"));
                continue;
            }
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            warnings.push(format!("malformed Kiro task file for task #{file_id}"));
            continue;
        };
        if value.get("id").and_then(parse_task_id) != Some(file_id) {
            warnings.push(format!("Kiro task id mismatch for task #{file_id}"));
            continue;
        }
        let status = match value.get("status").and_then(Value::as_str) {
            Some("pending") => TaskStatus::Pending,
            Some("in_progress") => TaskStatus::InProgress,
            Some("completed") => TaskStatus::Completed,
            _ => TaskStatus::Unknown,
        };
        tasks.push(ThreadTask {
            id: file_id,
            status,
        });
        let modified = fs::metadata(&task_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(DateTime::<Utc>::from);
        observed_at = max_time(observed_at, modified);
    }
    if tasks.is_empty() {
        return (None, None);
    }
    tasks.sort_by_key(|task| task.id);
    (Some(TaskProgress { tasks }), observed_at)
}

fn parse_task_id(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn read_acp(home: &Path) -> KiroSnapshot {
    let mut out = KiroSnapshot::default();
    let sessions = home.join("sessions");
    let workspaces = match immediate_entries(&sessions, &mut out.warnings) {
        Some(entries) => entries,
        None => return out,
    };
    for workspace in workspaces {
        if !workspace.is_dir() || workspace.file_name().and_then(|v| v.to_str()) == Some("cli") {
            continue;
        }
        let workers = match immediate_entries(&workspace, &mut out.warnings) {
            Some(entries) => entries,
            None => continue,
        };
        for worker in workers {
            if !worker.is_dir()
                || !worker
                    .file_name()
                    .and_then(|v| v.to_str())
                    .is_some_and(|v| v.starts_with("sess_"))
            {
                continue;
            }
            let session_path = worker.join("session.json");
            if is_symlink(&session_path) {
                out.warnings.push(format!(
                    "skipping symlinked Kiro session: {}",
                    session_path.display()
                ));
                continue;
            }
            let Some(root) = read_json(&session_path, &mut out.warnings) else {
                continue;
            };
            if root.get("schemaVersion").and_then(Value::as_str) != Some("1.0.0")
                || root.get("dataModelVersion").and_then(Value::as_i64) != Some(1)
            {
                out.warnings.push(format!(
                    "unsupported Kiro ACP session schema: {}",
                    session_path.display()
                ));
                continue;
            }
            let Some(id) = string_at(&root, &["id"]).filter(|v| v.starts_with("sess_")) else {
                out.warnings.push(format!(
                    "Kiro ACP session has no valid id: {}",
                    session_path.display()
                ));
                continue;
            };
            let data = &root;
            if data
                .get("workspacePaths")
                .and_then(Value::as_array)
                .is_none()
                && data.get("rootPaths").and_then(Value::as_array).is_none()
            {
                out.warnings.push(format!(
                    "unsupported Kiro ACP session fields: {}",
                    session_path.display()
                ));
                continue;
            }
            let cwd = first_string(data, &["workspacePaths", "rootPaths"]);
            let created_at = string_at(data, &["createdAt"])
                .as_deref()
                .and_then(parse_rfc3339);
            let updated_at = string_at(data, &["lastModifiedAt"])
                .as_deref()
                .and_then(parse_rfc3339);
            let model = string_at(data, &["modelId"]);
            let effort = string_at(data, &["effortLevel"]);
            let role = string_at(data, &["agentMode"]);
            let status = string_at(data, &["status"]);
            let messages_path = worker.join("messages.jsonl");
            let (activity, mut terminal, lifecycle_state, latest_message) =
                read_acp_messages(&messages_path, &mut out.warnings);
            let latest_lifecycle = activity
                .iter()
                .filter(|item| item.kind == "turn_start" || item.kind == "turn_end")
                .filter_map(|item| item.ts)
                .max();
            let metadata_is_current = match (updated_at, latest_lifecycle) {
                (Some(modified), Some(observed)) => modified >= observed,
                (Some(_), None) | (None, None) => true,
                (None, Some(_)) => false,
            };
            let metadata_failed = status.as_deref() == Some("failed");
            let metadata_failure_is_newer = metadata_failed
                && match (updated_at, terminal.as_ref().and_then(|(_, ts)| *ts)) {
                    (Some(modified), Some(observed)) => modified >= observed,
                    (Some(_), None) | (None, None) => true,
                    (None, Some(_)) => false,
                };
            if metadata_failure_is_newer {
                terminal = Some((RolloutTerminalEvent::Failed, updated_at));
            }
            let (state, lifecycle_at) =
                if metadata_is_current && matches!(status.as_deref(), Some("idle" | "failed")) {
                    (ThreadState::Idle, updated_at)
                } else {
                    (lifecycle_state, latest_lifecycle)
                };
            let recency_at = [
                updated_at,
                latest_message,
                terminal.as_ref().and_then(|(_, ts)| *ts),
            ]
            .into_iter()
            .flatten()
            .max();
            out.threads.push(KiroThreadRecord {
                id: format!("kiro:{id}"),
                created_at,
                updated_at,
                recency_at,
                cwd,
                nickname: None,
                role,
                source_kind: "kiro_acp".to_string(),
                model,
                effort,
                state,
                lifecycle_at,
                terminal,
                activity,
                context_usage: None,
                context_usage_at: None,
                task_progress: None,
                task_progress_at: None,
                task_progress_detail: Some(
                    "Task plans are unavailable for this Kiro session format".to_string(),
                ),
                path: Some(session_path),
                warnings: Vec::new(),
            });
        }
    }
    out
}

fn read_acp_messages(path: &Path, warnings: &mut Vec<String>) -> AcpMessageParse {
    if is_symlink_or_parent(path) {
        warnings.push(format!(
            "skipping symlinked Kiro ACP event log: {}",
            path.display()
        ));
        return (Vec::new(), None, ThreadState::Unknown, None);
    }
    let Ok(raw) = fs::read_to_string(path) else {
        return (Vec::new(), None, ThreadState::Unknown, None);
    };
    let mut events = Vec::<(
        Option<DateTime<Utc>>,
        usize,
        RolloutActivity,
        Option<(RolloutTerminalEvent, Option<DateTime<Utc>>)>,
    )>::new();
    for (sequence, line) in raw.lines().enumerate() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            warnings.push(format!("malformed Kiro ACP event in {}", path.display()));
            continue;
        };
        let timestamp = value.get("timestamp").and_then(parse_rfc3339_value);
        let payload = value.get("payload").unwrap_or(&value);
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let mut tool_name = None;
        let mut status = None;
        let mut terminal = None;
        match kind {
            "tool_call" => {
                tool_name = string_at(payload, &["toolName"]);
                status = string_at(payload, &["status"]);
            }
            "tool_result" => {
                status = payload
                    .get("success")
                    .and_then(Value::as_bool)
                    .map(|ok| if ok { "success" } else { "failed" }.to_string());
            }
            "turn_end" => {
                let reason = string_at(payload, &["stopReason"])
                    .or_else(|| string_at(payload, &["stop_reason"]));
                terminal = match reason.as_deref() {
                    Some("end_turn") => Some((RolloutTerminalEvent::Completed, timestamp)),
                    Some("error") => Some((RolloutTerminalEvent::Failed, timestamp)),
                    _ => None,
                };
            }
            _ => {}
        }
        if !kind.is_empty() {
            events.push((
                timestamp,
                sequence,
                RolloutActivity {
                    kind: kind.to_string(),
                    tool_name,
                    status,
                    ts: timestamp,
                },
                terminal,
            ));
        }
    }
    events.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut activity = Vec::new();
    let mut latest_terminal = None;
    let mut state = ThreadState::Unknown;
    for (_, _, event, terminal) in events {
        if event.kind == "turn_start" {
            state = ThreadState::Running;
        }
        if event.kind == "turn_end" && terminal.is_none() {
            state = ThreadState::Unknown;
        }
        if let Some(candidate) = terminal {
            latest_terminal = Some(candidate);
            state = match candidate.0 {
                RolloutTerminalEvent::Completed | RolloutTerminalEvent::Failed => ThreadState::Idle,
                RolloutTerminalEvent::Interrupted => ThreadState::Unknown,
            };
        }
        activity.push(event);
    }
    let latest = activity.iter().filter_map(|v| v.ts).max();
    (activity, latest_terminal, state, latest)
}

fn read_classic_db(path: &Path) -> KiroSnapshot {
    let mut out = KiroSnapshot::default();
    if !path.exists() {
        return out;
    }
    if is_symlink_or_parent(path) {
        out.warnings.push(format!(
            "skipping symlinked Kiro database: {}",
            path.display()
        ));
        return out;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = match Connection::open_with_flags(path, flags) {
        Ok(conn) => conn,
        Err(err) => {
            out.warnings.push(format!(
                "unable to open Kiro database {}: {err}",
                path.display()
            ));
            return out;
        }
    };
    if let Err(err) = conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 250;") {
        out.warnings.push(format!(
            "unable to configure Kiro database {}: {err}",
            path.display()
        ));
        return out;
    }
    let mut seen = HashSet::new();
    for table in ["conversations_v2", "conversations"] {
        if !table_exists(&conn, table) {
            continue;
        }
        let sql = if table == "conversations_v2" {
            "SELECT key, conversation_id, value, created_at, updated_at FROM conversations_v2"
                .to_string()
        } else {
            "SELECT key, value FROM conversations".to_string()
        };
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(err) => {
                out.warnings
                    .push(format!("unable to read Kiro table {table}: {err}"));
                continue;
            }
        };
        if table == "conversations_v2" {
            let rows = stmt.query_map([], |row| {
                let key: String = row.get(0)?;
                let conversation_id: Option<String> = row.get(1).ok();
                let value: String = row.get(2)?;
                let created: Option<i64> = row.get(3).ok();
                let updated: Option<i64> = row.get(4).ok();
                Ok((key, conversation_id, value, created, updated))
            });
            let Ok(rows) = rows else { continue };
            for row in rows.flatten() {
                append_classic_row(&mut out, &mut seen, path, table, row);
            }
        } else {
            let rows = stmt.query_map([], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, None, value, None, None))
            });
            let Ok(rows) = rows else { continue };
            for row in rows.flatten() {
                append_classic_row(&mut out, &mut seen, path, table, row);
            }
        }
    }
    out
}

fn append_classic_row(
    out: &mut KiroSnapshot,
    seen: &mut HashSet<String>,
    path: &Path,
    table: &str,
    row: (String, Option<String>, String, Option<i64>, Option<i64>),
) {
    let (key, column_conversation_id, raw, created, updated) = row;
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        out.warnings
            .push(format!("malformed Kiro conversation value in {table}"));
        return;
    };
    let Some(conversation_id) = column_conversation_id
        .or_else(|| string_at(&value, &["conversation_id"]))
        .filter(|v| !v.is_empty())
    else {
        return;
    };
    let id = format!("kiro:{conversation_id}");
    if !seen.insert(id.clone()) {
        return;
    }
    let cwd = string_at(&value, &["cwd"]).or_else(|| (!key.is_empty()).then_some(key.clone()));
    let created_at = created.and_then(parse_millis).or_else(|| {
        string_at(&value, &["created_at"])
            .as_deref()
            .and_then(parse_rfc3339)
    });
    let updated_at = updated.and_then(parse_millis).or_else(|| {
        string_at(&value, &["updated_at"])
            .as_deref()
            .and_then(parse_rfc3339)
    });
    let model = string_at(&value, &["model_info", "model_id"]);
    let (activity, terminal, latest) = classic_history(&value);
    out.threads.push(KiroThreadRecord {
        id,
        created_at,
        updated_at,
        recency_at: [updated_at, latest].into_iter().flatten().max(),
        cwd,
        nickname: None,
        role: None,
        source_kind: "kiro_cli".to_string(),
        model,
        effort: None,
        state: ThreadState::Unknown,
        lifecycle_at: None,
        terminal,
        activity,
        context_usage: None,
        context_usage_at: None,
        task_progress: None,
        task_progress_at: None,
        task_progress_detail: Some(
            "Task plans are unavailable for this Kiro session format".to_string(),
        ),
        path: Some(path.to_path_buf()),
        warnings: Vec::new(),
    });
}

fn classic_history(value: &Value) -> ClassicHistoryParse {
    let mut activity = Vec::new();
    if let Some(history) = value.get("history").and_then(Value::as_array) {
        for item in history {
            if let Some(user) = item.get("user") {
                let timestamp = string_at(user, &["timestamp"])
                    .as_deref()
                    .and_then(parse_rfc3339);
                activity.push(RolloutActivity {
                    kind: "user".to_string(),
                    tool_name: None,
                    status: None,
                    ts: timestamp,
                });
            }
            if let Some(assistant) = item.get("assistant") {
                let timestamp = string_at(assistant, &["timestamp"])
                    .as_deref()
                    .and_then(parse_rfc3339);
                if let Some(tool_uses) = assistant
                    .get("ToolUse")
                    .and_then(|v| v.get("tool_uses"))
                    .and_then(Value::as_array)
                {
                    for tool in tool_uses {
                        activity.push(RolloutActivity {
                            kind: "tool_use".to_string(),
                            tool_name: string_at(tool, &["name"]),
                            status: None,
                            ts: timestamp,
                        });
                    }
                } else {
                    activity.push(RolloutActivity {
                        kind: "assistant".to_string(),
                        tool_name: None,
                        status: None,
                        ts: timestamp,
                    });
                }
            }
        }
    }
    let latest = activity.iter().filter_map(|v| v.ts).max();
    (activity, None, latest)
}

fn immediate_entries(dir: &Path, warnings: &mut Vec<String>) -> Option<Vec<PathBuf>> {
    if !dir.exists() {
        return None;
    }
    if is_symlink_or_parent(dir) {
        warnings.push(format!(
            "skipping symlinked Kiro directory: {}",
            dir.display()
        ));
        return None;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            warnings.push(format!(
                "unable to read Kiro directory {}: {err}",
                dir.display()
            ));
            return None;
        }
    };
    let mut paths = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if is_symlink(&path) {
                warnings.push(format!("skipping symlinked Kiro entry: {}", path.display()));
                None
            } else {
                Some(path)
            }
        })
        .collect::<Vec<_>>();
    paths.sort();
    Some(paths)
}

fn read_json(path: &Path, warnings: &mut Vec<String>) -> Option<Value> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            warnings.push(format!(
                "unable to read Kiro session {}: {err}",
                path.display()
            ));
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(_) => {
            warnings.push(format!("malformed Kiro session JSON: {}", path.display()));
            None
        }
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn is_symlink_or_parent(path: &Path) -> bool {
    is_symlink(path) || path.parent().is_some_and(is_symlink)
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1",
        [table],
        |row| row.get::<_, i32>(0),
    )
    .is_ok()
}

fn default_classic_db() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::data_local_dir().map(|dir| dir.join("kiro-cli").join("data.sqlite3"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::data_local_dir().map(|dir| dir.join("kiro-cli").join("data.sqlite3"))
    }
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(ToString::to_string)
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|v| v.with_timezone(&Utc))
}

fn parse_rfc3339_value(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str().and_then(parse_rfc3339)
}

fn parse_unix_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let number = value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))?;
    Utc.timestamp_opt(number, 0).single()
}

fn parse_millis(value: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(value).single()
}

fn max_time(
    current: Option<DateTime<Utc>>,
    candidate: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (current, candidate) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}
