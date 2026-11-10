use std::fs;
use std::process::Command;

use chrono::{Duration, Utc};
use codex_agent_monitor::copilot::{self, UsageSnapshotStatus};
use tempfile::TempDir;

fn event(kind: &str, timestamp: chrono::DateTime<Utc>, data: serde_json::Value) -> String {
    serde_json::json!({
        "type": kind,
        "timestamp": timestamp.to_rfc3339(),
        "data": data
    })
    .to_string()
}

fn copilot_home(temp: &TempDir, session: &str, log: &str) -> std::path::PathBuf {
    let dir = temp
        .path()
        .join("copilot")
        .join("session-state")
        .join(session);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("events.jsonl"), log).unwrap();
    dir.parent().unwrap().parent().unwrap().to_path_buf()
}

fn usage_receipt(
    timestamp: chrono::DateTime<Utc>,
    source: &str,
    sequence: u64,
    id: &str,
    input: serde_json::Value,
    output: serde_json::Value,
) -> String {
    event(
        "session.usage_record",
        timestamp,
        serde_json::json!({
            "usage": {
                "accounting": {"sourceSessionId": source, "sequence": sequence, "usageId": id},
                "inputTokens": input,
                "outputTokens": output,
                "cacheReadTokens": 99,
                "reasoningTokens": 77
            },
            "prompt": "PRIVATE_RECEIPT_PROMPT"
        }),
    )
}

fn shutdown_with_watermark(timestamp: chrono::DateTime<Utc>, sequence: u64) -> String {
    event(
        "session.shutdown",
        timestamp,
        serde_json::json!({
            "usageAccountingWatermarks": {"source-1": sequence},
            "modelMetrics": {"model-a": {"usage": {"inputTokens": 50, "outputTokens": 5}}}
        }),
    )
}

fn open_turn_log(session: &str) -> String {
    let now = Utc::now();
    format!(
        "{}\n{}",
        event(
            "session.start",
            now,
            serde_json::json!({"sessionId":session})
        ),
        event(
            "assistant.turn_start",
            now,
            serde_json::json!({"turnId":"turn-1"})
        )
    )
}

fn add_live_lock(session_dir: &std::path::Path) {
    let pid = std::process::id();
    fs::write(
        session_dir.join(format!("inuse.{pid}.lock")),
        pid.to_string(),
    )
    .unwrap();
}

#[test]
fn unmatched_turn_requires_a_valid_live_session_lock() {
    use codex_agent_monitor::model::ThreadState;

    let temp = TempDir::new().unwrap();
    let home = copilot_home(&temp, "live-turn", &open_turn_log("live-turn"));
    let session_dir = home.join("session-state/live-turn");
    add_live_lock(&session_dir);
    assert_eq!(
        copilot::read_snapshot_at(&home).threads[0].state,
        ThreadState::Running
    );

    let current_pid = std::process::id().to_string();
    let cases = [
        ("dead", "4294967294".to_string(), "4294967294".to_string()),
        (
            "malformed",
            "not-a-pid".to_string(),
            "not-a-pid".to_string(),
        ),
        ("mismatch", current_pid, "1".to_string()),
    ];
    for (name, filename_pid, contents) in cases {
        let temp = TempDir::new().unwrap();
        let home = copilot_home(&temp, "uncertain", &open_turn_log("uncertain"));
        let dir = home.join("session-state/uncertain");
        fs::write(dir.join(format!("inuse.{filename_pid}.lock")), contents).unwrap();
        assert_eq!(
            copilot::read_snapshot_at(&home).threads[0].state,
            ThreadState::Unknown,
            "{name}"
        );
    }

    let temp = TempDir::new().unwrap();
    let home = copilot_home(&temp, "no-lock", &open_turn_log("no-lock"));
    assert_eq!(
        copilot::read_snapshot_at(&home).threads[0].state,
        ThreadState::Unknown
    );
}

#[cfg(unix)]
#[test]
fn symlinked_lock_is_not_running_evidence() {
    use codex_agent_monitor::model::ThreadState;
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let home = copilot_home(&temp, "symlink-lock", &open_turn_log("symlink-lock"));
    let dir = home.join("session-state/symlink-lock");
    let pid = std::process::id();
    let target = temp.path().join("lock-target");
    fs::write(&target, pid.to_string()).unwrap();
    symlink(target, dir.join(format!("inuse.{pid}.lock"))).unwrap();
    assert_eq!(
        copilot::read_snapshot_at(&home).threads[0].state,
        ThreadState::Unknown
    );
}

#[test]
fn open_turn_with_malformed_tail_or_matching_end_is_not_running() {
    use codex_agent_monitor::model::ThreadState;
    let now = Utc::now();
    let end = event(
        "assistant.turn_end",
        now,
        serde_json::json!({"turnId":"turn-1"}),
    );
    for (session, suffix) in [("bad-tail", "{bad json"), ("finished", end.as_str())] {
        let temp = TempDir::new().unwrap();
        let mut log = open_turn_log(session);
        log.push('\n');
        log.push_str(suffix);
        let home = copilot_home(&temp, session, &log);
        add_live_lock(&home.join("session-state").join(session));
        let snapshot = copilot::read_snapshot_at(&home);
        let state = snapshot.threads[0].state.clone();
        assert_ne!(state, ThreadState::Running, "{session}");
        assert_eq!(state, ThreadState::Unknown, "{session}");
    }
}

#[test]
fn monitor_running_filter_includes_only_lock_backed_open_copilot_turn() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("copilot-home");
    let session_dir = home.join("session-state/filtered");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("events.jsonl"), open_turn_log("filtered")).unwrap();
    add_live_lock(&session_dir);
    let output = Command::new(env!("CARGO_BIN_EXE_codex-agent-monitor"))
        .args(["--provider", "copilot", "--copilot-home"])
        .arg(&home)
        .args(["--state", "running", "--json", "probe"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = json["threads"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["thread_id"], "copilot:filtered");
    assert_eq!(rows[0]["state"], "running");
}

#[test]
fn shared_session_state_extracts_safe_metadata_and_cumulative_shutdown_usage() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let start = event(
        "session.start",
        now - Duration::minutes(1),
        serde_json::json!({
            "sessionId": "session-1",
            "startTime": (now - Duration::minutes(1)).to_rfc3339(),
            "selectedModel": "gpt-model-alias",
            "context": {"cwd": "/work/project"},
            "prompt": "PRIVATE_PROMPT_MUST_NOT_SURVIVE"
        }),
    );
    let shutdown = event(
        "session.shutdown",
        now,
        serde_json::json!({
            "currentModel": "gpt-model-alias",
            "modelMetrics": {
                "gpt-model-alias": {"usage": {
                    "inputTokens": 10,
                    "outputTokens": 4,
                    "cacheReadTokens": 2,
                    "cacheWriteTokens": 1,
                    "reasoningTokens": 3,
                    "currentTokens": 9000
                }}
            },
            "privateToolResult": "PRIVATE_TOOL_RESULT_MUST_NOT_SURVIVE"
        }),
    );
    let home = copilot_home(&temp, "session-1", &format!("{start}\n{shutdown}\n"));

    let snapshot = copilot::read_snapshot_at(&home);
    assert_eq!(snapshot.threads.len(), 1);
    let thread = &snapshot.threads[0];
    assert_eq!(thread.id, "copilot:session-1");
    assert_eq!(thread.model.as_deref(), Some("gpt-model-alias"));
    assert_eq!(thread.cwd.as_deref(), Some("/work/project"));
    assert_eq!(thread.state, codex_agent_monitor::model::ThreadState::Idle);
    assert_eq!(thread.token_usage.as_ref().unwrap().input_tokens, Some(10));
    assert_eq!(thread.token_usage.as_ref().unwrap().output_tokens, Some(4));
    assert_eq!(
        thread.token_usage.as_ref().unwrap().status,
        UsageSnapshotStatus::Latest
    );
    let serialized =
        serde_json::to_string(&thread.activity.iter().map(|a| &a.kind).collect::<Vec<_>>())
            .unwrap();
    assert!(!serialized.contains("PRIVATE_PROMPT"));
    assert!(!serialized.contains("PRIVATE_TOOL_RESULT"));
    assert!(!format!("{thread:?}").contains("PRIVATE_PROMPT"));
    assert!(!format!("{thread:?}").contains("PRIVATE_TOOL_RESULT"));
}

#[test]
fn resumed_sessions_do_not_report_old_shutdown_counters_as_current() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let start = event(
        "session.start",
        now - Duration::minutes(3),
        serde_json::json!({"sessionId":"resume-1"}),
    );
    let shutdown = event(
        "session.shutdown",
        now - Duration::minutes(2),
        serde_json::json!({
            "modelMetrics": {"model-a": {"usage": {"inputTokens": 5}}}
        }),
    );
    let resume = event(
        "session.resume",
        now - Duration::minutes(1),
        serde_json::json!({}),
    );
    let home = copilot_home(
        &temp,
        "resume-1",
        &format!("{start}\n{shutdown}\n{resume}\n"),
    );
    let snapshot = copilot::read_snapshot_at(&home);
    assert_eq!(
        snapshot.threads[0].token_usage.as_ref().unwrap().status,
        UsageSnapshotStatus::Superseded
    );
    assert_eq!(
        snapshot.threads[0].state,
        codex_agent_monitor::model::ThreadState::Unknown
    );
}

#[test]
fn resumed_session_exposes_only_latest_request_as_separate_json_observation() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let start = event(
        "session.start",
        now - Duration::minutes(4),
        serde_json::json!({"sessionId":"request-1"}),
    );
    let shutdown = event(
        "session.shutdown",
        now - Duration::minutes(3),
        serde_json::json!({
            "modelMetrics":{"model-a":{"usage":{"inputTokens":100,"outputTokens":50}}}
        }),
    );
    let old_request = event(
        "session.usage_record",
        now - Duration::minutes(2),
        serde_json::json!({
            "usage":{"inputTokens":10,"outputTokens":5},
            "prompt":"PRIVATE_OLD_PROMPT"
        }),
    );
    let resume = event(
        "session.resume",
        now - Duration::minutes(1),
        serde_json::json!({}),
    );
    let latest_request = event(
        "session.usage_record",
        now,
        serde_json::json!({
            "usage":{"inputTokens":23,"outputTokens":7,"cacheReadTokens":2},
            "assistantText":"PRIVATE_ASSISTANT_TEXT"
        }),
    );
    let home = copilot_home(
        &temp,
        "request-1",
        &format!("{start}\n{shutdown}\n{old_request}\n{resume}\n{latest_request}\n"),
    );
    let thread = &copilot::read_snapshot_at(&home).threads[0];
    assert_eq!(
        thread.token_usage.as_ref().unwrap().status,
        UsageSnapshotStatus::Superseded
    );
    let request = thread.last_request_usage.as_ref().unwrap();
    assert_eq!(request.input_tokens, Some(23));
    assert_eq!(request.output_tokens, Some(7));
    assert_eq!(request.cache_read_tokens, Some(2));
    assert!(!request.followed_by_unverified_records);
    assert!(!format!("{thread:?}").contains("PRIVATE_OLD_PROMPT"));
    assert!(!format!("{thread:?}").contains("PRIVATE_ASSISTANT_TEXT"));
}

#[test]
fn six_receipts_across_resume_and_duplicate_shutdowns_prove_session_total() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let amounts = [
        (50_000, 200),
        (40_000, 200),
        (30_000, 200),
        (25_000, 200),
        (28_000, 200),
        (25_069, 248),
    ];
    let mut events = vec![
        event(
            "session.start",
            now - Duration::minutes(9),
            serde_json::json!({"sessionId":"receipt-session"}),
        ),
        usage_receipt(
            now - Duration::minutes(8),
            "source-1",
            1,
            "usage-1",
            amounts[0].0.into(),
            amounts[0].1.into(),
        ),
    ];
    events.push(shutdown_with_watermark(now - Duration::minutes(7), 1));
    events.push(event(
        "session.resume",
        now - Duration::minutes(6),
        serde_json::json!({}),
    ));
    events.push(shutdown_with_watermark(now - Duration::minutes(5), 1));
    for (index, (input, output)) in amounts.into_iter().enumerate().skip(1) {
        events.push(usage_receipt(
            now - Duration::minutes(4 - index as i64),
            "source-1",
            (index + 1) as u64,
            &format!("usage-{}", index + 1),
            input.into(),
            output.into(),
        ));
    }
    // Replayed lines are deduplicated by their persisted usage identity.
    events.push(usage_receipt(
        now,
        "source-1",
        6,
        "usage-6",
        25_069.into(),
        248.into(),
    ));
    let home = copilot_home(&temp, "receipt-session", &events.join("\n"));
    let snapshot = copilot::read_snapshot_at(&home);
    let thread = &snapshot.threads[0];
    let usage = thread.token_usage.as_ref().unwrap();
    assert_eq!(usage.status, UsageSnapshotStatus::VerifiedReceipts);
    assert_eq!(usage.input_tokens, Some(198_069));
    assert_eq!(usage.output_tokens, Some(1_248));
    assert_eq!(
        thread.last_request_usage.as_ref().unwrap().input_tokens,
        Some(25_069)
    );
    assert!(!format!("{thread:?}").contains("PRIVATE_RECEIPT_PROMPT"));
}

#[test]
fn incomplete_receipt_ledgers_stay_unknown() {
    let now = Utc::now();
    let cases = [
        (
            "sequence gap",
            vec![
                usage_receipt(now, "source-1", 1, "a", 1.into(), 1.into()),
                usage_receipt(now, "source-1", 3, "c", 1.into(), 1.into()),
            ],
        ),
        (
            "conflicting duplicate id",
            vec![
                usage_receipt(now, "source-1", 1, "same", 1.into(), 1.into()),
                usage_receipt(now, "source-1", 2, "same", 2.into(), 1.into()),
            ],
        ),
        (
            "missing output",
            vec![event(
                "session.usage_record",
                now,
                serde_json::json!({
                    "usage":{
                        "accounting":{"sourceSessionId":"source-1","sequence":1,"usageId":"missing-output"},
                        "inputTokens":1
                    }
                }),
            )],
        ),
        (
            "missing usage object",
            vec![
                usage_receipt(now, "source-1", 1, "valid", 1.into(), 1.into()),
                event("session.usage_record", now, serde_json::json!({})),
            ],
        ),
        (
            "input overflow",
            vec![
                usage_receipt(now, "source-1", 1, "a", u64::MAX.into(), 1.into()),
                usage_receipt(now, "source-1", 2, "b", 1.into(), 1.into()),
            ],
        ),
        (
            "watermark beyond coverage",
            vec![
                usage_receipt(now, "source-1", 1, "a", 1.into(), 1.into()),
                shutdown_with_watermark(now, 2),
            ],
        ),
        (
            "decreasing watermark",
            vec![
                usage_receipt(now, "source-1", 1, "a", 1.into(), 1.into()),
                usage_receipt(now, "source-1", 2, "b", 1.into(), 1.into()),
                shutdown_with_watermark(now, 2),
                shutdown_with_watermark(now, 1),
            ],
        ),
    ];
    for (name, mut events) in cases {
        let session_id = name.replace(' ', "-");
        events.insert(
            0,
            event(
                "session.start",
                now - Duration::minutes(1),
                serde_json::json!({"sessionId": session_id}),
            ),
        );
        events.push(shutdown_with_watermark(now, 1));
        let temp = TempDir::new().unwrap();
        let home = copilot_home(&temp, name.replace(' ', "-").as_str(), &events.join("\n"));
        let thread = &copilot::read_snapshot_at(&home).threads[0];
        assert_ne!(
            thread.token_usage.as_ref().map(|usage| usage.status),
            Some(UsageSnapshotStatus::VerifiedReceipts),
            "{name}"
        );
    }
}

#[test]
fn receipt_ledger_without_verified_session_start_stays_unknown() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let log = format!(
        "{}\n{}",
        usage_receipt(now, "source-1", 1, "usage-1", 10.into(), 2.into()),
        shutdown_with_watermark(now, 1)
    );
    let home = copilot_home(&temp, "missing-start", &log);
    let thread = &copilot::read_snapshot_at(&home).threads[0];
    assert_eq!(
        thread.token_usage.as_ref().unwrap().status,
        UsageSnapshotStatus::Uncertain
    );
}

#[test]
fn unmatched_fresh_turn_start_does_not_claim_a_live_process() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let start = event(
        "session.start",
        now,
        serde_json::json!({"sessionId":"possibly-crashed"}),
    );
    let turn = event(
        "assistant.turn_start",
        now,
        serde_json::json!({"turnId":"turn-1"}),
    );
    let home = copilot_home(&temp, "possibly-crashed", &format!("{start}\n{turn}\n"));
    let snapshot = copilot::read_snapshot_at(&home);
    let thread = snapshot
        .threads
        .iter()
        .find(|thread| thread.id == "copilot:possibly-crashed")
        .unwrap();
    assert_eq!(
        thread.state,
        codex_agent_monitor::model::ThreadState::Unknown
    );
}

#[test]
fn malformed_tail_keeps_lifecycle_unknown_and_omits_content() {
    let temp = TempDir::new().unwrap();
    let now = Utc::now();
    let start = event(
        "session.start",
        now,
        serde_json::json!({"sessionId":"bad-tail"}),
    );
    let home = copilot_home(
        &temp,
        "bad-tail",
        &format!("{start}\n{{\"message\":\"PRIVATE\""),
    );
    let snapshot = copilot::read_snapshot_at(&home);
    assert_eq!(
        snapshot.threads[0].state,
        codex_agent_monitor::model::ThreadState::Unknown
    );
    assert!(!format!("{:?}", snapshot.threads).contains("PRIVATE"));
}

#[test]
fn cli_provider_copilot_filters_raw_id_and_emits_safe_snapshot_from_isolated_home() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("isolated-user-home");
    let copilot_home = temp.path().join("copilot-home");
    let session_dir = copilot_home.join("session-state").join("cli-session-42");
    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(&home).unwrap();
    let now = Utc::now();
    let start = event(
        "session.start",
        now - Duration::minutes(1),
        serde_json::json!({
            "sessionId":"cli-session-42",
            "selectedModel":"copilot/model-a",
            "prompt":"PRIVATE_CLI_PROMPT"
        }),
    );
    let shutdown = event(
        "session.shutdown",
        now,
        serde_json::json!({
            "modelMetrics":{"copilot/model-a":{"usage":{"inputTokens":23,"outputTokens":7}}},
            "toolResult":"PRIVATE_CLI_TOOL_RESULT"
        }),
    );
    fs::write(
        session_dir.join("events.jsonl"),
        format!("{start}\n{shutdown}\n"),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_codex-agent-monitor"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("APPDATA", home.join("AppData/Roaming"))
        .args([
            "--provider",
            "copilot",
            "--copilot-home",
            copilot_home.to_str().unwrap(),
            "--thread",
            "cli-session-42",
            "--json",
            "probe",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("PRIVATE_CLI_PROMPT"));
    assert!(!stdout.contains("PRIVATE_CLI_TOOL_RESULT"));
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["query"]["provider"], "copilot");
    let rows = json["threads"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["thread_id"], "copilot:cli-session-42");
    assert_eq!(row["state"], "idle");
    assert_eq!(
        row["model"]["requested"]["value"]["model"],
        "copilot/model-a"
    );
    assert_eq!(row["token_usage"]["value"]["input_tokens"], 23);
    assert_eq!(row["token_usage"]["value"]["output_tokens"], 7);
}
