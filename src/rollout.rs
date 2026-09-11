use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct RolloutParseResult {
    pub warnings: Vec<String>,
    pub canonical_parent: Option<String>,
    pub requested_model: Option<RolloutModelObservation>,
    pub token_usage: Option<RolloutTokenUsageObservation>,
    pub account_usage: Option<RolloutAccountUsageObservation>,
    pub canonical_nickname: Option<String>,
    pub canonical_role: Option<String>,
    pub canonical_agent_path: Option<String>,
    pub canonical_meta_seen: bool,
    pub canonical_meta_timestamp: Option<DateTime<Utc>>,
    pub activity: Vec<RolloutActivity>,
    pub final_state: RolloutStateHint,
    pub last_terminal_event: Option<RolloutTerminalObservation>,
    pub final_state_at: Option<DateTime<Utc>>,
    pub latest_lifecycle_at: Option<DateTime<Utc>>,
    pub latest_activity_at: Option<DateTime<Utc>>,
    pub tail_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutTerminalEvent {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutTerminalObservation {
    pub event: RolloutTerminalEvent,
    pub observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RolloutAccountUsageObservation {
    pub primary: Option<RolloutAccountUsageWindow>,
    pub secondary: Option<RolloutAccountUsageWindow>,
    pub source: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RolloutAccountUsageWindow {
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutTokenUsageObservation {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub source: String,
    pub observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutModelObservation {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub source: String,
    pub observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RolloutStateHint {
    #[default]
    Unknown,
    Running,
    Idle,
    Interrupted,
    Failed,
    TurnCompleted,
}

#[derive(Debug, Clone)]
pub struct RolloutActivity {
    pub kind: String,
    pub tool_name: Option<String>,
    pub status: Option<String>,
    pub ts: Option<DateTime<Utc>>,
}

pub fn parse_rollout_file(
    path: &Path,
    thread_id: &str,
    allow_partial_final: bool,
) -> RolloutParseResult {
    let mut result = RolloutParseResult {
        final_state: RolloutStateHint::Unknown,
        ..Default::default()
    };

    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            result
                .warnings
                .push(format!("rollout missing or unreadable: {}", path.display()));
            return result;
        }
    };

    let mut reader = BufReader::new(file);
    let mut raw_line = String::new();
    let mut line_number = 0usize;
    let mut saw_unterminated_final = false;

    loop {
        raw_line.clear();
        match reader.read_line(&mut raw_line) {
            Ok(0) => break,
            Ok(bytes_read) => {
                line_number += 1;
                let terminated = raw_line.ends_with('\n');
                let is_final = bytes_read > 0 && !terminated;
                let line = raw_line.trim_end_matches(&['\n', '\r'][..]);

                if line.trim().is_empty() {
                    continue;
                }

                match serde_json::from_str::<Value>(line) {
                    Ok(value) => {
                        if is_final && !allow_partial_final {
                            saw_unterminated_final = true;
                            result.warnings.push(format!(
                                "rollout tail appears truncated in {}",
                                path.display()
                            ));
                            continue;
                        }
                        process_record(&value, thread_id, &mut result);
                    }
                    Err(err) => {
                        if is_final {
                            saw_unterminated_final = true;
                            if allow_partial_final {
                                result.warnings.push(format!(
                                    "malformed trailing JSONL in {}: {err}",
                                    path.display()
                                ));
                            } else {
                                result.warnings.push(format!(
                                    "rollout tail appears truncated in {}: {err}",
                                    path.display()
                                ));
                            }
                        } else {
                            result.warnings.push(format!(
                                "malformed JSONL line {} in {}: {err}",
                                line_number,
                                path.display()
                            ));
                        }
                    }
                }
            }
            Err(err) => {
                result
                    .warnings
                    .push(format!("rollout read error in {}: {err}", path.display()));
                break;
            }
        }
    }

    if saw_unterminated_final {
        result.tail_truncated = true;
    }

    result
}

fn process_record(value: &Value, thread_id: &str, result: &mut RolloutParseResult) {
    let event_type = extract_event_type(value);
    let payload = value.get("payload");
    let record_timestamp = extract_timestamp(value);

    if !result.canonical_meta_seen {
        if let Some(fields) = extract_session_meta_fields(value, &event_type, thread_id) {
            result.canonical_nickname = fields.nickname;
            result.canonical_role = fields.role;
            result.canonical_agent_path = fields.agent_path;
            result.canonical_parent = fields.parent_thread_id;
            result.canonical_meta_timestamp = record_timestamp;
            result.canonical_meta_seen = true;
        }
    }

    let is_older_copy = result
        .canonical_meta_timestamp
        .as_ref()
        .zip(record_timestamp.as_ref())
        .is_some_and(|(boundary, ts)| result.canonical_meta_seen && ts < boundary);

    if let Some(model) =
        extract_model_observation(event_type.as_deref(), &payload, record_timestamp)
    {
        if !is_older_copy {
            result.requested_model = Some(model);
        }
    }

    if let Some(token_usage) =
        extract_token_usage_observation(event_type.as_deref(), &payload, record_timestamp)
    {
        if !is_older_copy {
            result.token_usage = Some(token_usage);
        }
    }

    if let Some(account_usage) =
        extract_account_usage_observation(event_type.as_deref(), &payload, record_timestamp)
    {
        if !is_older_copy
            && result
                .account_usage
                .as_ref()
                .is_none_or(|existing| account_usage.observed_at >= existing.observed_at)
        {
            result.account_usage = Some(account_usage);
        }
    }

    if !is_older_copy {
        if let Some(activity) = build_activity(event_type.clone(), payload, record_timestamp) {
            update_latest_timestamp(&mut result.latest_activity_at, activity.ts);
            result.activity.push(activity);
        }
    }

    if !is_older_copy {
        match event_type.as_deref() {
            Some("task_started") | Some("turn_started") => {
                result.final_state = RolloutStateHint::Running;
                result.final_state_at = record_timestamp;
                update_latest_timestamp(&mut result.latest_lifecycle_at, record_timestamp);
            }
            Some("turn_aborted") => {
                result.final_state = RolloutStateHint::Interrupted;
                result.final_state_at = record_timestamp;
                result.last_terminal_event = Some(RolloutTerminalObservation {
                    event: RolloutTerminalEvent::Interrupted,
                    observed_at: record_timestamp,
                });
                update_latest_timestamp(&mut result.latest_lifecycle_at, record_timestamp);
            }
            Some("task_complete") => {
                let is_failed = payload
                    .as_ref()
                    .and_then(|p| {
                        p.get("error")
                            .or_else(|| p.get("payload").and_then(|nested| nested.get("error")))
                    })
                    .is_some_and(|error| !error.is_null());
                if is_failed {
                    result.final_state = RolloutStateHint::Failed;
                    result.last_terminal_event = Some(RolloutTerminalObservation {
                        event: RolloutTerminalEvent::Failed,
                        observed_at: record_timestamp,
                    });
                } else {
                    result.final_state = RolloutStateHint::TurnCompleted;
                    result.last_terminal_event = Some(RolloutTerminalObservation {
                        event: RolloutTerminalEvent::Completed,
                        observed_at: record_timestamp,
                    });
                }
                result.final_state_at = record_timestamp;
                update_latest_timestamp(&mut result.latest_lifecycle_at, record_timestamp);
            }
            Some("task_done") => {
                result.final_state = RolloutStateHint::TurnCompleted;
                result.final_state_at = record_timestamp;
                result.last_terminal_event = Some(RolloutTerminalObservation {
                    event: RolloutTerminalEvent::Completed,
                    observed_at: record_timestamp,
                });
                update_latest_timestamp(&mut result.latest_lifecycle_at, record_timestamp);
            }
            Some("agent_idle") | Some("idle") => {
                result.final_state = RolloutStateHint::Idle;
                result.final_state_at = record_timestamp;
                update_latest_timestamp(&mut result.latest_lifecycle_at, record_timestamp);
            }
            _ => {}
        }
    }
}

fn update_latest_timestamp(slot: &mut Option<DateTime<Utc>>, candidate: Option<DateTime<Utc>>) {
    if let Some(candidate) = candidate {
        if slot.is_none_or(|existing| candidate > existing) {
            *slot = Some(candidate);
        }
    }
}

fn extract_token_usage_observation(
    event_type: Option<&str>,
    payload: &Option<&Value>,
    observed_at: Option<DateTime<Utc>>,
) -> Option<RolloutTokenUsageObservation> {
    if event_type != Some("token_count") {
        return None;
    }
    let target = match payload {
        Some(value) => value.get("payload").unwrap_or(value),
        None => &Value::Null,
    };
    let info = pick_key_paths(target, &[&["info"], &["payload", "info"]])?;
    let totals = pick_key_paths(
        info,
        &[
            &["total_token_usage"],
            &["totalTokenUsage"],
            &["payload", "total_token_usage"],
            &["payload", "totalTokenUsage"],
        ],
    )?;
    let totals = totals.as_object()?;
    let field = |name: &str| lookup_ci(totals, name).and_then(parse_token_count);
    let input_tokens = field("input_tokens");
    let cached_input_tokens = field("cached_input_tokens");
    let cache_write_input_tokens = field("cache_write_input_tokens");
    let output_tokens = field("output_tokens");
    let reasoning_output_tokens = field("reasoning_output_tokens");
    let total_tokens = field("total_tokens");
    let context_window = lookup_ci(info.as_object()?, "model_context_window")
        .or_else(|| lookup_ci(target.as_object()?, "model_context_window"))
        .and_then(parse_token_count);

    // A token_count record is useful only when it contributes at least one
    // valid usage counter. Missing or malformed counters remain unknown.
    if input_tokens.is_none()
        && cached_input_tokens.is_none()
        && cache_write_input_tokens.is_none()
        && output_tokens.is_none()
        && reasoning_output_tokens.is_none()
        && total_tokens.is_none()
    {
        return None;
    }

    Some(RolloutTokenUsageObservation {
        input_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        context_window,
        source: "rollout.token_count".to_string(),
        observed_at,
    })
}

fn extract_account_usage_observation(
    event_type: Option<&str>,
    payload: &Option<&Value>,
    observed_at: Option<DateTime<Utc>>,
) -> Option<RolloutAccountUsageObservation> {
    if event_type != Some("token_count") {
        return None;
    }
    let observed_at = observed_at?;
    let target = match payload {
        Some(value) => value.get("payload").unwrap_or(value),
        None => &Value::Null,
    };
    let rate_limits = pick_key_paths(target, &[&["rate_limits"], &["payload", "rate_limits"]])?;
    let object = rate_limits.as_object()?;
    match lookup_ci(object, "limit_id") {
        None | Some(Value::Null) => {}
        Some(value)
            if value
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case("codex")) => {}
        Some(_) => return None,
    }

    let primary = lookup_ci(object, "primary").and_then(parse_account_usage_window);
    let secondary = lookup_ci(object, "secondary").and_then(parse_account_usage_window);
    if primary.is_none() && secondary.is_none() {
        return None;
    }

    Some(RolloutAccountUsageObservation {
        primary,
        secondary,
        source: "rollout.rate_limits".to_string(),
        observed_at,
    })
}

fn parse_account_usage_window(value: &Value) -> Option<RolloutAccountUsageWindow> {
    let object = value.as_object()?;
    let used_percent = lookup_ci(object, "used_percent")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0);
    let window_minutes = lookup_ci(object, "window_minutes")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0);
    let resets_at = lookup_ci(object, "resets_at").and_then(parse_timestamp_value);
    Some(RolloutAccountUsageWindow {
        used_percent,
        window_minutes,
        resets_at,
    })
}

fn parse_token_count(value: &Value) -> Option<u64> {
    value.as_u64()
}

fn extract_event_type(value: &Value) -> Option<String> {
    let top_type = value.get("type").and_then(Value::as_str)?;
    if top_type == "event_msg" {
        if let Some(payload) = value.get("payload") {
            if let Some(inner) = lookup_ci(payload.as_object()?, "type").and_then(Value::as_str) {
                return Some(inner.to_string());
            }
            if let Some(inner) = payload
                .get("event")
                .and_then(|v| lookup_ci(v.as_object()?, "type"))
                .and_then(Value::as_str)
            {
                return Some(inner.to_string());
            }
        }
        return Some("event_msg".to_string());
    }
    Some(top_type.to_string())
}

fn extract_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let ts = value.get("timestamp").or_else(|| value.get("time"))?;
    parse_timestamp_value(ts)
}

fn parse_timestamp_value(ts: &Value) -> Option<DateTime<Utc>> {
    match ts {
        Value::Number(num) => num
            .as_i64()
            .or_else(|| num.as_u64().and_then(|v| i64::try_from(v).ok()))
            .and_then(parse_number_timestamp),
        Value::String(raw) => {
            if let Ok(raw) = raw.parse::<i64>() {
                parse_number_timestamp(raw)
            } else if let Ok(raw) = raw.parse::<f64>() {
                parse_number_timestamp(raw.round() as i64)
            } else {
                chrono::DateTime::parse_from_rfc3339(raw)
                    .ok()
                    .map(|x| x.with_timezone(&Utc))
            }
        }
        Value::Null => None,
        Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

fn parse_number_timestamp(raw: i64) -> Option<DateTime<Utc>> {
    if raw > 10_000_000_000 {
        Utc.timestamp_millis_opt(raw).single()
    } else {
        Utc.timestamp_opt(raw, 0).single()
    }
}

fn extract_session_meta_fields(
    value: &Value,
    event_type: &Option<String>,
    thread_id: &str,
) -> Option<SessionMetaFields> {
    if event_type.as_deref()? != "session_meta" {
        return None;
    }
    let payload = value.get("payload").unwrap_or(value);
    if !matches_session_meta_id(payload, thread_id) {
        return None;
    }
    let nickname = pick_key_paths(
        payload,
        &[
            &["nickname"],
            &["nick"],
            &["agent_nickname"],
            &["agent", "nickname"],
            &["payload", "nickname"],
            &["payload", "nick"],
            &["payload", "agent_nickname"],
        ],
    )
    .and_then(Value::as_str)
    .map(ToString::to_string);
    let role = pick_key_paths(
        payload,
        &[
            &["role"],
            &["agent_type"],
            &["agentType"],
            &["agent", "type"],
            &["payload", "role"],
            &["payload", "agent_type"],
            &["payload", "agentType"],
            &["payload", "type"],
        ],
    )
    .and_then(Value::as_str)
    .map(ToString::to_string);
    let agent_path = pick_key_paths(
        payload,
        &[
            &["agent_path"],
            &["agentPath"],
            &["payload", "agent_path"],
            &["payload", "agentPath"],
        ],
    )
    .and_then(Value::as_str)
    .map(ToString::to_string);
    let parent_thread_id = extract_parent_from_session_meta(value, event_type, thread_id);
    Some(SessionMetaFields {
        nickname,
        role,
        agent_path,
        parent_thread_id,
    })
}

#[derive(Debug, Clone)]
struct SessionMetaFields {
    nickname: Option<String>,
    role: Option<String>,
    agent_path: Option<String>,
    parent_thread_id: Option<String>,
}

fn extract_parent_from_session_meta(
    value: &Value,
    event_type: &Option<String>,
    thread_id: &str,
) -> Option<String> {
    if event_type.as_deref()? != "session_meta" {
        return None;
    }
    let payload = value.get("payload").unwrap_or(value);
    if !matches_session_meta_id(payload, thread_id) {
        return None;
    }
    pick_key_paths(
        payload,
        &[
            &["parent_thread_id"],
            &["parentThreadId"],
            &["payload", "parent_thread_id"],
            &["payload", "parentThreadId"],
            &["source", "subagent", "thread_spawn", "parent_thread_id"],
            &["source", "subAgent", "threadSpawn", "parentThreadId"],
            &["thread_spawn", "parent_thread_id"],
            &["threadSpawn", "parentThreadId"],
            &["source", "thread_spawn", "parent_thread_id"],
            &["source", "threadSpawn", "parentThreadId"],
        ],
    )
    .and_then(Value::as_str)
    .map(ToString::to_string)
}

fn matches_session_meta_id(payload: &Value, thread_id: &str) -> bool {
    pick_key_paths(payload, &[&["id"], &["payload", "id"]])
        .and_then(Value::as_str)
        .is_some_and(|canonical| canonical.eq_ignore_ascii_case(thread_id))
}

fn extract_model_observation(
    event_type: Option<&str>,
    payload: &Option<&Value>,
    observed_at: Option<DateTime<Utc>>,
) -> Option<RolloutModelObservation> {
    match event_type {
        Some("turn_context") => extract_turn_context(payload),
        Some("thread_settings_applied") => extract_thread_settings(payload),
        _ => None,
    }
    .map(|(model, effort)| RolloutModelObservation {
        model,
        effort,
        source: format!("rollout.{}", event_type.unwrap_or("unknown")),
        observed_at,
    })
}

fn extract_turn_context(payload: &Option<&Value>) -> Option<(Option<String>, Option<String>)> {
    let target = match payload {
        Some(value) => value.get("payload").unwrap_or(value),
        None => &Value::Null,
    };
    let model = pick_key_paths(
        target,
        &[
            &["model"],
            &["payload", "model"],
            &["turn_context", "model"],
        ],
    )
    .and_then(Value::as_str)
    .map(ToString::to_string);
    let effort = pick_key_paths(
        target,
        &[
            &["reasoning_effort"],
            &["reasoningEffort"],
            &["effort"],
            &["payload", "reasoning_effort"],
            &["payload", "reasoningEffort"],
            &["payload", "effort"],
            &["thread_settings", "reasoning_effort"],
            &["thread_settings", "reasoningEffort"],
        ],
    )
    .and_then(Value::as_str)
    .map(ToString::to_string);
    if model.is_none() && effort.is_none() {
        None
    } else {
        Some((model, effort))
    }
}

fn extract_thread_settings(payload: &Option<&Value>) -> Option<(Option<String>, Option<String>)> {
    let target = match payload {
        Some(value) => value.get("payload").unwrap_or(value),
        None => &Value::Null,
    };
    let settings = pick_key_paths(
        target,
        &[
            &["thread_settings"],
            &["settings"],
            &["payload", "thread_settings"],
            &["payload", "settings"],
        ],
    )
    .unwrap_or(target);
    let settings = if settings.is_object() {
        settings
    } else {
        target
    };

    let model = pick_key_paths(
        settings,
        &[
            &["model"],
            &["payload", "model"],
            &["thread_settings", "model"],
            &["settings", "model"],
        ],
    )
    .and_then(Value::as_str)
    .map(ToString::to_string);
    let effort = pick_key_paths(
        settings,
        &[
            &["reasoning_effort"],
            &["reasoningEffort"],
            &["effort"],
            &["payload", "reasoning_effort"],
            &["payload", "reasoningEffort"],
            &["thread_settings", "reasoning_effort"],
            &["thread_settings", "reasoningEffort"],
        ],
    )
    .and_then(Value::as_str)
    .map(ToString::to_string);
    if model.is_none() && effort.is_none() {
        None
    } else {
        Some((model, effort))
    }
}

fn build_activity(
    kind: Option<String>,
    payload: Option<&Value>,
    ts: Option<DateTime<Utc>>,
) -> Option<RolloutActivity> {
    let kind = kind?;
    let payload = payload
        .and_then(|v| v.get("payload"))
        .or(payload)
        .unwrap_or(&Value::Null);
    let tool_name = payload
        .as_object()
        .and_then(|obj| {
            lookup_ci_object(obj, "tool")
                .and_then(Value::as_object)
                .and_then(|tool| {
                    lookup_ci_object(tool, "name")
                        .or_else(|| lookup_ci_object(tool, "tool_name"))
                        .or_else(|| lookup_ci_object(tool, "toolName"))
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
        })
        .or_else(|| {
            lookup_ci_value(payload, "tool_name")
                .or_else(|| lookup_ci_value(payload, "toolName"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .or_else(|| {
            payload.as_object().and_then(|o| {
                o.iter().find_map(|(k, v)| {
                    if k.eq_ignore_ascii_case("tool") {
                        if let Some(tool) = v.get("name").or_else(|| v.get("tool_name")) {
                            return tool.as_str().map(ToString::to_string);
                        }
                        None
                    } else {
                        None
                    }
                })
            })
        });
    let status = payload
        .as_object()
        .and_then(|obj| lookup_ci(obj, "status"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    Some(RolloutActivity {
        kind,
        tool_name,
        status,
        ts,
    })
}

pub fn pick_key_paths<'a>(value: &'a Value, paths: &[&[&str]]) -> Option<&'a Value> {
    for path in paths {
        let mut cursor = value;
        let mut ok = true;
        for key in *path {
            cursor = match cursor {
                Value::Object(map) => {
                    if let Some(found) = map.get(*key) {
                        found
                    } else if let Some(found) = map.iter().find_map(|(name, val)| {
                        if name.eq_ignore_ascii_case(key) {
                            Some(val)
                        } else {
                            None
                        }
                    }) {
                        found
                    } else {
                        ok = false;
                        break;
                    }
                }
                _ => {
                    ok = false;
                    break;
                }
            };
        }
        if ok {
            return Some(cursor);
        }
    }
    None
}

fn lookup_ci<'a>(object: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.get(key).or_else(|| {
        object
            .iter()
            .find_map(|(name, val)| name.eq_ignore_ascii_case(key).then_some(val))
    })
}

fn lookup_ci_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|obj| lookup_ci(obj, key))
}

fn lookup_ci_object<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Option<&'a Value> {
    object.get(key).or_else(|| {
        object
            .iter()
            .find_map(|(name, val)| name.eq_ignore_ascii_case(key).then_some(val))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    #[test]
    fn parse_rollout_tracks_state_lifecycle_and_parent() {
        let path = Path::new("tests/fixtures/rollout/sample.jsonl");
        let result = parse_rollout_file(path, "thread-root", false);
        assert_eq!(result.final_state, RolloutStateHint::TurnCompleted);
        assert_eq!(
            result.last_terminal_event,
            Some(RolloutTerminalObservation {
                event: RolloutTerminalEvent::Completed,
                observed_at: Some(Utc.timestamp_opt(1_720_000_001, 0).unwrap()),
            })
        );
        assert_eq!(
            result.requested_model,
            Some(RolloutModelObservation {
                model: Some("gpt-3".into()),
                effort: Some("low".into()),
                source: "rollout.thread_settings_applied".into(),
                observed_at: Some(Utc.timestamp_opt(1_720_000_001, 0).unwrap()),
            })
        );
        assert_eq!(result.canonical_parent.as_deref(), Some("parent-ignored"));
        assert_eq!(result.canonical_nickname, None);
    }

    #[test]
    fn idle_event_ends_running_state_and_stream_order_controls_final_state_time() {
        let mut file = tempfile::NamedTempFile::new().expect("temporary rollout");
        writeln!(file, r#"{{"timestamp":1720000200,"type":"task_started"}}"#).expect("write start");
        writeln!(file, r#"{{"timestamp":1720000100,"type":"agent_idle"}}"#).expect("write idle");
        let result = parse_rollout_file(file.path(), "thread", false);
        assert_eq!(result.final_state, RolloutStateHint::Idle);
        assert_eq!(
            result.final_state_at,
            Some(Utc.timestamp_opt(1_720_000_100, 0).unwrap())
        );
        assert_eq!(
            result.latest_lifecycle_at,
            Some(Utc.timestamp_opt(1_720_000_200, 0).unwrap())
        );
    }

    #[test]
    fn parse_rollout_final_partial_treated_as_warning_when_allowed() {
        let path = Path::new("tests/fixtures/rollout/partial.jsonl");
        let result = parse_rollout_file(path, "thread-a", true);
        assert_eq!(result.final_state, RolloutStateHint::Running);
        assert!(result.tail_truncated);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("malformed trailing JSONL")));
    }

    #[test]
    fn parse_rollout_accepts_clean_unterminated_tail_in_one_shot() {
        let path = Path::new("tests/fixtures/rollout/unterminated.jsonl");
        let result = parse_rollout_file(path, "thread-clean", true);
        assert_eq!(result.final_state, RolloutStateHint::Running);
        assert!(!result.tail_truncated);
    }

    #[test]
    fn parse_rollout_rejects_clean_unterminated_tail_in_watch_mode() {
        let path = Path::new("tests/fixtures/rollout/unterminated.jsonl");
        let result = parse_rollout_file(path, "thread-clean", false);
        assert_eq!(result.final_state, RolloutStateHint::Running);
        assert!(result.tail_truncated);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("rollout tail appears truncated")));
    }

    #[test]
    fn parse_rollout_skips_malformed_middle_valid_following() {
        let path = Path::new("tests/fixtures/rollout/malformed_middle_then_valid.jsonl");
        let result = parse_rollout_file(path, "thread-middle", false);
        assert_eq!(result.final_state, RolloutStateHint::TurnCompleted);
        assert_eq!(
            result.requested_model,
            Some(RolloutModelObservation {
                model: Some("gpt-3.1".into()),
                effort: Some("high".into()),
                source: "rollout.turn_context".into(),
                observed_at: Some(Utc.timestamp_opt(1_720_000_603, 0).unwrap()),
            })
        );
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("malformed JSONL line 2")));
    }

    #[test]
    fn parse_rollout_tracks_latest_token_usage_and_context_window() {
        let path = Path::new("tests/fixtures/rollout/token_usage.jsonl");
        let result = parse_rollout_file(path, "token-thread", false);
        assert_eq!(
            result.token_usage,
            Some(RolloutTokenUsageObservation {
                input_tokens: Some(30),
                cached_input_tokens: Some(5),
                cache_write_input_tokens: Some(2),
                output_tokens: Some(8),
                reasoning_output_tokens: Some(6),
                total_tokens: Some(51),
                context_window: Some(128_000),
                source: "rollout.token_count".into(),
                observed_at: Some(Utc.timestamp_opt(1_720_010_002, 0).unwrap()),
            })
        );
    }

    #[test]
    fn parse_rollout_preserves_partial_usage_and_ignores_malformed_usage() {
        let path = Path::new("tests/fixtures/rollout/token_usage_partial.jsonl");
        let result = parse_rollout_file(path, "token-thread", false);
        let usage = result.token_usage.expect("latest valid usage");
        assert_eq!(usage.input_tokens, Some(17));
        assert_eq!(usage.total_tokens, Some(22));
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.context_window, None);
    }

    #[test]
    fn parse_rollout_ignores_stale_token_usage_before_canonical_boundary() {
        let path = Path::new("tests/fixtures/rollout/token_usage_stale_copy.jsonl");
        let result = parse_rollout_file(path, "token-stale-thread", false);
        assert_eq!(
            result
                .token_usage
                .as_ref()
                .and_then(|usage| usage.total_tokens),
            Some(12)
        );
        assert_eq!(
            result
                .token_usage
                .as_ref()
                .and_then(|usage| usage.input_tokens),
            Some(10)
        );
    }

    #[test]
    fn parse_rollout_ignores_token_count_without_valid_counters() {
        let path = Path::new("tests/fixtures/rollout/token_usage_malformed.jsonl");
        let result = parse_rollout_file(path, "missing-thread", false);
        assert!(result.token_usage.is_none());
    }

    #[test]
    fn parse_rollout_extracts_latest_account_usage_and_ignores_stale_or_other_buckets() {
        let path = Path::new("tests/fixtures/rollout/rate_limits_usage.jsonl");
        let result = parse_rollout_file(path, "usage-thread", false);
        let usage = result.account_usage.expect("account usage");
        assert_eq!(usage.observed_at, Utc.timestamp_opt(101, 0).unwrap());
        let primary = usage.primary.expect("primary window");
        assert_eq!(primary.used_percent, Some(44.0));
        assert_eq!(primary.window_minutes, Some(10_080));
        assert_eq!(primary.resets_at, Some(Utc.timestamp_opt(2000, 0).unwrap()));
        let secondary = usage.secondary.expect("secondary window");
        assert_eq!(secondary.used_percent, Some(25.0));
        assert_eq!(secondary.window_minutes, Some(300));
    }

    #[test]
    fn parse_rollout_keeps_malformed_account_fields_unknown_and_accepts_null_info() {
        let path = Path::new("tests/fixtures/rollout/rate_limits_malformed.jsonl");
        let result = parse_rollout_file(path, "missing-thread", false);
        let usage = result.account_usage.expect("account usage");
        let primary = usage.primary.expect("primary window");
        assert_eq!(primary.used_percent, None);
        assert_eq!(primary.window_minutes, None);
        assert_eq!(primary.resets_at, None);
        assert_eq!(usage.secondary, None);
    }

    #[test]
    fn parse_rollout_parent_from_session_meta_requires_matching_id() {
        let path = Path::new("tests/fixtures/rollout/session_meta.jsonl");
        let result = parse_rollout_file(path, "child", true);
        assert_eq!(result.canonical_parent.as_deref(), Some("root"));
        let mismatch = parse_rollout_file(path, "other", true);
        assert!(mismatch.canonical_parent.is_none());
    }

    #[test]
    fn parse_rollout_session_meta_is_frozen_to_first_matching_record() {
        let path = Path::new("tests/fixtures/rollout/session_meta_first_match.jsonl");
        let result = parse_rollout_file(path, "child", true);
        assert_eq!(result.canonical_nickname.as_deref(), Some("FirstNick"));
        assert_eq!(result.canonical_role.as_deref(), Some("FirstRole"));
        assert_eq!(result.canonical_parent.as_deref(), Some("first-parent"));
        assert_eq!(result.canonical_agent_path.as_deref(), Some("/first/path"));
    }

    #[test]
    fn parse_rollout_session_meta_freeze_applies_even_when_first_record_is_empty() {
        let path = Path::new("tests/fixtures/rollout/session_meta_empty_then_conflict.jsonl");
        let result = parse_rollout_file(path, "child-empty", true);
        assert!(result.canonical_meta_seen);
        assert_eq!(
            result.canonical_meta_timestamp,
            Some(Utc.timestamp_opt(1_720_009_000, 0).unwrap())
        );
        assert_eq!(result.canonical_nickname, None);
        assert_eq!(result.canonical_role, None);
        assert_eq!(result.canonical_parent, None);
    }

    #[test]
    fn parse_rollout_ignores_stale_records_before_boundary() {
        let path = Path::new("tests/fixtures/rollout/fork_history_guard.jsonl");
        let result = parse_rollout_file(path, "fork-child", false);
        assert_eq!(result.final_state, RolloutStateHint::Running);
        assert_eq!(
            result.requested_model,
            Some(RolloutModelObservation {
                model: Some("child-model".into()),
                effort: Some("high".into()),
                source: "rollout.turn_context".into(),
                observed_at: Some(Utc.timestamp_opt(1_720_010_020, 0).unwrap()),
            })
        );
        let copy_tool_calls = result
            .activity
            .iter()
            .filter(|activity| activity.tool_name.as_deref() == Some("copied_tool"))
            .count();
        let child_tool_calls = result
            .activity
            .iter()
            .filter(|activity| activity.tool_name.as_deref() == Some("child_tool"))
            .count();
        let stale_turn_context = result
            .activity
            .iter()
            .filter(|activity| activity.kind == "turn_context")
            .count();
        let stale_task_complete = result
            .activity
            .iter()
            .filter(|activity| activity.kind == "task_complete")
            .count();
        assert_eq!(copy_tool_calls, 0);
        assert_eq!(child_tool_calls, 1);
        assert_eq!(stale_turn_context, 1);
        assert_eq!(stale_task_complete, 0);
        assert!(result
            .activity
            .iter()
            .any(|activity| activity.kind == "session_meta"));
    }

    #[test]
    fn parse_rollout_parent_from_session_meta_direct_field() {
        let path = Path::new("tests/fixtures/rollout/session_meta_direct_parent.jsonl");
        let result = parse_rollout_file(path, "child-direct", true);
        assert_eq!(result.canonical_parent.as_deref(), Some("direct-root"));
        assert_eq!(result.canonical_nickname.as_deref(), Some("DirectChild"));
    }

    #[test]
    fn parse_rollout_settings_then_turn_context_uses_latest_event() {
        let path = Path::new("tests/fixtures/rollout/settings_then_turn_context.jsonl");
        let result = parse_rollout_file(path, "thread-a", false);
        assert_eq!(
            result.requested_model,
            Some(RolloutModelObservation {
                model: Some("gpt-4.1".into()),
                effort: Some("high".into()),
                source: "rollout.turn_context".into(),
                observed_at: None,
            })
        );
    }

    #[test]
    fn parse_rollout_turn_context_after_settings_uses_latest_turn_context() {
        let path = Path::new("tests/fixtures/rollout/turn_context_after_settings.jsonl");
        let result = parse_rollout_file(path, "thread-b", false);
        assert_eq!(
            result.requested_model,
            Some(RolloutModelObservation {
                model: Some("gpt-4.5".into()),
                effort: Some("medium".into()),
                source: "rollout.turn_context".into(),
                observed_at: None,
            })
        );
    }

    #[test]
    fn parse_rollout_turn_context_then_settings_uses_latest_settings() {
        let path = Path::new("tests/fixtures/rollout/turn_context_then_settings.jsonl");
        let result = parse_rollout_file(path, "thread-c", false);
        assert_eq!(
            result.requested_model,
            Some(RolloutModelObservation {
                model: Some("gpt-4.8".into()),
                effort: Some("medium".into()),
                source: "rollout.thread_settings_applied".into(),
                observed_at: Some(Utc.timestamp_opt(1_720_000_131, 0).unwrap()),
            })
        );
    }

    #[test]
    fn parse_rollout_ignores_unknown_fields_and_supports_aliases() {
        let path = Path::new("tests/fixtures/rollout/unknown_fields.jsonl");
        let result = parse_rollout_file(path, "thread-d", false);
        assert_eq!(
            result.requested_model,
            Some(RolloutModelObservation {
                model: Some("gpt-4".into()),
                effort: Some("high".into()),
                source: "rollout.turn_context".into(),
                observed_at: Some(Utc.timestamp_opt(1_720_000_012, 0).unwrap()),
            })
        );
    }

    #[test]
    fn parse_rollout_activity_keeps_safe_fields_only() {
        let path = Path::new("tests/fixtures/rollout/privacy_activity.jsonl");
        let result = parse_rollout_file(path, "thread-secure", false);
        assert_eq!(result.activity.len(), 2);
        assert_eq!(result.activity[0].kind, "tool_call");
        assert_eq!(result.activity[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(result.activity[0].status.as_deref(), Some("done"));

        let first = format!("{:?}", result.activity[0]);
        let second = format!("{:?}", result.activity[1]);
        assert!(!first.contains("SECRET"));
        assert!(!second.contains("SECRET"));
    }
}
