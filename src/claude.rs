//! Read-only ingestion for local Claude Code CLI transcripts.
//!
//! This module deliberately parses only telemetry needed by the monitor. It
//! never retains prompts, message text, thinking, tool inputs or results,
//! attachments, custom titles, agent names, or file snapshots.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::model::{
    AccountUsage, AccountUsageWindow, Confidence, EvidenceSource, Observed, ThreadState,
};
use crate::rollout::{RolloutActivity, RolloutTerminalEvent, RolloutTokenUsageObservation};

const MAX_SESSION_FILES: usize = 500;
const MAX_TRANSCRIPT_LINES: usize = 200_000;
const MAX_ACTIVITY: usize = 25;
const MAX_PLAN_USAGE_BYTES: u64 = 2 * 1024 * 1024;
const FIVE_HOUR_WINDOW_MINUTES: u64 = 300;
const SEVEN_DAY_WINDOW_MINUTES: u64 = 7 * 24 * 60;

#[derive(Debug, Clone)]
pub struct ClaudeThreadRecord {
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
    pub token_usage: Option<RolloutTokenUsageObservation>,
    pub path: Option<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeSnapshot {
    pub threads: Vec<ClaudeThreadRecord>,
    pub warnings: Vec<String>,
}

/// A registered interactive session whose owning process is still visible.
#[derive(Debug, Clone, Default)]
struct LiveSession {
    cwd: Option<String>,
    entrypoint: Option<String>,
}

pub fn resolve_claude_home(cli_home: Option<&str>) -> Result<(PathBuf, bool)> {
    if let Some(home) = cli_home {
        return Ok((PathBuf::from(home), true));
    }
    if let Some(home) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok((PathBuf::from(home), true));
    }
    let home = dirs::home_dir()
        .map(|path| path.join(".claude"))
        .ok_or_else(|| {
            anyhow::anyhow!("Unable to resolve home dir for default CLAUDE_CONFIG_DIR")
        })?;
    Ok((home, false))
}

pub fn read_snapshot(home: &Path) -> ClaudeSnapshot {
    let mut out = ClaudeSnapshot::default();
    let projects = home.join("projects");
    if !projects.is_dir() {
        out.warnings.push(format!(
            "Claude Code projects directory missing: {}",
            projects.display()
        ));
        return out;
    }
    let live = read_live_sessions(&home.join("sessions"), &mut out.warnings);
    let mut transcripts = collect_transcripts(&projects, &mut out.warnings);
    transcripts.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    transcripts.truncate(MAX_SESSION_FILES);
    for (path, _) in transcripts {
        match read_transcript(&path, &live) {
            Ok(Some(record)) => out.threads.push(record),
            Ok(None) => {}
            Err(err) => out
                .warnings
                .push(format!("Unable to read {}: {err}", path.display())),
        }
    }
    out
}

/// `projects/<encoded-cwd>/<session-uuid>.jsonl`; nested directories hold
/// per-session scratch data the monitor does not read.
fn collect_transcripts(
    projects: &Path,
    warnings: &mut Vec<String>,
) -> Vec<(PathBuf, DateTime<Utc>)> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(err) => {
            warnings.push(format!(
                "Unable to list Claude Code projects {}: {err}",
                projects.display()
            ));
            return out;
        }
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(entry.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let modified = file
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .map(DateTime::<Utc>::from)
                .unwrap_or_else(Utc::now);
            out.push((path, modified));
        }
    }
    out
}

/// `sessions/<pid>.json` registers a running CLI process. Entries whose process
/// is gone are stale and do not make a session running.
fn read_live_sessions(dir: &Path, warnings: &mut Vec<String>) -> HashMap<String, LiveSession> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            warnings.push(format!(
                "Malformed Claude Code session registry: {}",
                path.display()
            ));
            continue;
        };
        let session_id = value.get("sessionId").and_then(Value::as_str);
        let pid = value
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok())
            .filter(|pid| *pid > 0);
        let (Some(session_id), Some(pid)) = (session_id, pid) else {
            continue;
        };
        if !crate::kiro::process_is_alive(pid) {
            continue;
        }
        out.insert(
            session_id.to_string(),
            LiveSession {
                cwd: string_field(&value, "cwd"),
                entrypoint: string_field(&value, "entrypoint"),
            },
        );
    }
    out
}

fn read_transcript(
    path: &Path,
    live: &HashMap<String, LiveSession>,
) -> Result<Option<ClaudeThreadRecord>> {
    let reader = BufReader::new(File::open(path)?);
    let mut acc = TranscriptAccumulator::default();
    let mut lines = 0_usize;
    for line in reader.lines() {
        let line = line?;
        lines += 1;
        if lines > MAX_TRANSCRIPT_LINES {
            acc.warnings.push(format!(
                "Claude Code transcript truncated after {MAX_TRANSCRIPT_LINES} lines: {}",
                path.display()
            ));
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(value) => acc.ingest(&value),
            Err(_) => acc.malformed += 1,
        }
    }
    let Some(session_id) = acc
        .session_id
        .clone()
        .or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };
    if acc.malformed > 0 {
        acc.warnings.push(format!(
            "Skipped {} malformed Claude Code transcript lines: {}",
            acc.malformed,
            path.display()
        ));
    }
    Ok(Some(acc.into_record(session_id, path, live)))
}

#[derive(Debug, Default)]
struct TranscriptAccumulator {
    session_id: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    entrypoint: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    first_at: Option<DateTime<Utc>>,
    last_at: Option<DateTime<Utc>>,
    terminal: Option<(RolloutTerminalEvent, Option<DateTime<Utc>>)>,
    activity: VecDeque<RolloutActivity>,
    counted_messages: HashSet<String>,
    usage: TokenTotals,
    saw_record: bool,
    malformed: usize,
    warnings: Vec<String>,
}

#[derive(Debug, Default, Clone, Copy)]
struct TokenTotals {
    input: u64,
    cached_input: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
    seen: bool,
}

impl TranscriptAccumulator {
    fn ingest(&mut self, value: &Value) {
        self.saw_record = true;
        if self.session_id.is_none() {
            self.session_id = string_field(value, "sessionId");
        }
        let ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        if let Some(ts) = ts {
            self.first_at = Some(self.first_at.map_or(ts, |first| first.min(ts)));
            self.last_at = Some(self.last_at.map_or(ts, |last| last.max(ts)));
        }
        // Sidechain records belong to a subagent turn; they must not overwrite
        // the session's own model, effort, or workspace identity.
        let is_sidechain = value
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_sidechain {
            if let Some(cwd) = string_field(value, "cwd") {
                self.cwd = Some(cwd);
            }
            if let Some(branch) = string_field(value, "gitBranch") {
                self.git_branch = Some(branch);
            }
            if let Some(entrypoint) = string_field(value, "entrypoint") {
                self.entrypoint = Some(entrypoint);
            }
        }
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => self.ingest_assistant(value, ts, is_sidechain),
            Some("user") => self.ingest_user(value, ts),
            Some("system") => self.push_activity(RolloutActivity {
                kind: "system".to_string(),
                tool_name: None,
                status: string_field(value, "subtype"),
                ts,
            }),
            _ => {}
        }
    }

    fn ingest_assistant(&mut self, value: &Value, ts: Option<DateTime<Utc>>, is_sidechain: bool) {
        let Some(message) = value.get("message") else {
            return;
        };
        if !is_sidechain {
            if let Some(model) = string_field(message, "model") {
                self.model = Some(model);
            }
            if let Some(effort) = string_field(value, "effort") {
                self.effort = Some(effort);
            }
        }
        // One API response is written as one record per content block, each
        // carrying the same usage; count each response id at most once.
        let message_id = string_field(message, "id");
        let uncounted = message_id
            .as_ref()
            .is_none_or(|id| self.counted_messages.insert(id.clone()));
        if uncounted {
            self.usage.add(message.get("usage"));
        }
        let stop_reason = string_field(message, "stop_reason");
        if stop_reason.as_deref() == Some("end_turn") {
            self.terminal = Some((RolloutTerminalEvent::Completed, ts));
        }
        self.push_activity(RolloutActivity {
            kind: "assistant_message".to_string(),
            tool_name: None,
            status: stop_reason,
            ts,
        });
        for block in content_blocks(message) {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                self.push_activity(RolloutActivity {
                    kind: "tool_use".to_string(),
                    tool_name: string_field(block, "name"),
                    status: None,
                    ts,
                });
            }
        }
    }

    fn ingest_user(&mut self, value: &Value, ts: Option<DateTime<Utc>>) {
        let Some(message) = value.get("message") else {
            return;
        };
        let mut tool_results = 0_usize;
        for block in content_blocks(message) {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                tool_results += 1;
                let status = match block.get("is_error").and_then(Value::as_bool) {
                    Some(true) => Some("error".to_string()),
                    Some(false) => Some("ok".to_string()),
                    None => None,
                };
                self.push_activity(RolloutActivity {
                    kind: "tool_result".to_string(),
                    tool_name: None,
                    status,
                    ts,
                });
            }
        }
        if tool_results == 0 {
            self.push_activity(RolloutActivity {
                kind: "user_message".to_string(),
                tool_name: None,
                status: None,
                ts,
            });
        }
    }

    fn push_activity(&mut self, activity: RolloutActivity) {
        if self.activity.len() == MAX_ACTIVITY {
            self.activity.pop_front();
        }
        self.activity.push_back(activity);
    }

    fn into_record(
        self,
        session_id: String,
        path: &Path,
        live: &HashMap<String, LiveSession>,
    ) -> ClaudeThreadRecord {
        let live_session = live.get(&session_id);
        let state = if live_session.is_some() {
            ThreadState::Running
        } else if self.saw_record {
            ThreadState::Idle
        } else {
            ThreadState::Unknown
        };
        let recency_at = self.last_at;
        ClaudeThreadRecord {
            id: format!("claude:{session_id}"),
            created_at: self.first_at,
            updated_at: self.last_at,
            recency_at,
            cwd: self
                .cwd
                .or_else(|| live_session.and_then(|session| session.cwd.clone())),
            nickname: self
                .git_branch
                .map(|branch| format!("Claude Code @ {branch}")),
            role: self
                .entrypoint
                .or_else(|| live_session.and_then(|session| session.entrypoint.clone())),
            source_kind: "claude_code".to_string(),
            model: self.model,
            effort: self.effort,
            state,
            lifecycle_at: self.last_at,
            terminal: self.terminal,
            activity: self.activity.into_iter().collect(),
            token_usage: self.usage.into_observation(recency_at),
            path: Some(path.to_path_buf()),
            warnings: self.warnings,
        }
    }
}

impl TokenTotals {
    fn add(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        let reasoning = usage
            .get("output_tokens_details")
            .and_then(|details| u64_field(details, "thinking_tokens"));
        let fields = [
            u64_field(usage, "input_tokens"),
            u64_field(usage, "cache_read_input_tokens"),
            u64_field(usage, "cache_creation_input_tokens"),
            u64_field(usage, "output_tokens"),
            reasoning,
        ];
        if fields.iter().all(Option::is_none) {
            return;
        }
        self.seen = true;
        self.input = self.input.saturating_add(fields[0].unwrap_or(0));
        self.cached_input = self.cached_input.saturating_add(fields[1].unwrap_or(0));
        self.cache_write = self.cache_write.saturating_add(fields[2].unwrap_or(0));
        self.output = self.output.saturating_add(fields[3].unwrap_or(0));
        self.reasoning = self.reasoning.saturating_add(fields[4].unwrap_or(0));
    }

    fn into_observation(
        self,
        observed_at: Option<DateTime<Utc>>,
    ) -> Option<RolloutTokenUsageObservation> {
        if !self.seen {
            return None;
        }
        let total = self
            .input
            .saturating_add(self.cached_input)
            .saturating_add(self.cache_write)
            .saturating_add(self.output);
        Some(RolloutTokenUsageObservation {
            input_tokens: Some(self.input),
            cached_input_tokens: Some(self.cached_input),
            cache_write_input_tokens: Some(self.cache_write),
            output_tokens: Some(self.output),
            reasoning_output_tokens: Some(self.reasoning),
            total_tokens: Some(total),
            // Claude Code does not persist the context window size.
            context_window: None,
            source: "claude_code.transcript".to_string(),
            observed_at,
        })
    }
}

fn content_blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.is_empty())
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn parse_rfc3339(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

/// Claude Code's desktop app records a rolling history of plan utilisation as
/// `{ "t": <ms>, "org": <id>, "u": { "fh": <pct>, "sd": <pct> } }` samples,
/// where `fh` is the five-hour window and `sd` the seven-day window. Only the
/// newest sample carrying at least one percentage is reported, and the `org`
/// account identifier is never read out of the file.
pub fn default_plan_usage_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("Claude").join("plan-usage-history.json"))
}

pub fn read_plan_usage(path: Option<&Path>) -> Observed<AccountUsage> {
    let Some(path) = path.map(PathBuf::from).or_else(default_plan_usage_path) else {
        return Observed::unknown();
    };
    let Ok(metadata) = fs::metadata(&path) else {
        return Observed::unknown();
    };
    if metadata.len() > MAX_PLAN_USAGE_BYTES {
        return Observed::unknown();
    }
    let Ok(raw) = fs::read_to_string(&path) else {
        return Observed::unknown();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Observed::unknown();
    };
    parse_plan_usage(&value)
}

pub fn parse_plan_usage(value: &Value) -> Observed<AccountUsage> {
    let Some(samples) = value.get("samples").and_then(Value::as_array) else {
        return Observed::unknown();
    };
    let newest = samples
        .iter()
        .filter_map(|sample| {
            let usage = sample.get("u")?;
            let five_hour = percent_field(usage, "fh");
            let seven_day = percent_field(usage, "sd");
            if five_hour.is_none() && seven_day.is_none() {
                return None;
            }
            let observed_at = sample
                .get("t")
                .and_then(Value::as_i64)
                .and_then(|ms| chrono::Utc.timestamp_millis_opt(ms).single());
            Some((observed_at, five_hour, seven_day))
        })
        .max_by_key(|(observed_at, _, _)| *observed_at);
    let Some((observed_at, five_hour, seven_day)) = newest else {
        return Observed::unknown();
    };
    Observed {
        value: Some(AccountUsage {
            primary: five_hour.map(|used_percent| AccountUsageWindow {
                used_percent: Some(used_percent),
                window_minutes: Some(FIVE_HOUR_WINDOW_MINUTES),
                // The desktop app records utilisation only, never a window start.
                resets_at: None,
            }),
            secondary: seven_day.map(|used_percent| AccountUsageWindow {
                used_percent: Some(used_percent),
                window_minutes: Some(SEVEN_DAY_WINDOW_MINUTES),
                resets_at: None,
            }),
        }),
        source: Some(EvidenceSource {
            kind: "claude.plan_usage_history".to_string(),
            detail: Some("Claude desktop plan utilisation sample".to_string()),
        }),
        observed_at,
        confidence: Confidence::Medium,
        detail: Some(
            "plan utilisation percentages only; the local history records no reset time"
                .to_string(),
        ),
    }
}

fn percent_field(usage: &Value, key: &str) -> Option<f64> {
    usage
        .get(key)
        .and_then(Value::as_f64)
        .filter(|percent| percent.is_finite() && *percent >= 0.0 && *percent <= 100.0)
}
