use std::fs;
use std::path::Path;

use codex_agent_monitor::claude::{parse_plan_usage, read_plan_usage, read_snapshot};
use codex_agent_monitor::cli::{FilterOpts, OutputFormat, Provider};
use codex_agent_monitor::model::ThreadState;
use codex_agent_monitor::observer::Monitor;
use tempfile::TempDir;

const SESSION: &str = "11111111-2222-3333-4444-555555555555";

fn filters(provider: Provider) -> FilterOpts {
    FilterOpts {
        provider,
        all: true,
        format: OutputFormat::Json,
        ..Default::default()
    }
}

fn write_transcript(home: &Path, session: &str, lines: &[String]) {
    let dir = home.join("projects").join("-Users-someone-repo");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{session}.jsonl")), lines.join("\n")).unwrap();
}

/// One API response is persisted as several records sharing `message.id`, each
/// repeating the same usage block.
fn response_records(
    message_id: &str,
    ts: &str,
    output_tokens: u64,
    sidechain: bool,
) -> Vec<String> {
    ["text", "tool_use"]
        .iter()
        .map(|block| {
            serde_json::json!({
                "type": "assistant",
                "sessionId": SESSION,
                "timestamp": ts,
                "cwd": "/Users/someone/repo",
                "gitBranch": "main",
                "entrypoint": "cli",
                "isSidechain": sidechain,
                "effort": if sidechain { "low" } else { "high" },
                "message": {
                    "id": message_id,
                    "model": if sidechain { "claude-haiku-4-5" } else { "claude-opus-5" },
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 10,
                        "cache_read_input_tokens": 100,
                        "cache_creation_input_tokens": 20,
                        "output_tokens": output_tokens,
                        "output_tokens_details": { "thinking_tokens": 5 }
                    },
                    "content": [
                        { "type": "text", "text": "SECRET_ASSISTANT_TEXT" },
                        { "type": "thinking", "thinking": "SECRET_THINKING" },
                        { "type": "tool_use", "name": "Bash", "input": { "command": "SECRET_COMMAND" } }
                    ]
                },
                "block": block
            })
            .to_string()
        })
        .collect()
}

fn fixture() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let mut lines = vec![
        serde_json::json!({
            "type": "user",
            "sessionId": SESSION,
            "timestamp": "2026-09-20T10:00:00.000Z",
            "cwd": "/Users/someone/repo",
            "gitBranch": "main",
            "entrypoint": "cli",
            "message": { "role": "user", "content": [{ "type": "text", "text": "SECRET_PROMPT" }] }
        })
        .to_string(),
        serde_json::json!({ "type": "custom-title", "sessionId": SESSION, "customTitle": "SECRET_TITLE" })
            .to_string(),
        serde_json::json!({ "type": "agent-name", "sessionId": SESSION, "agentName": "SECRET_AGENT" })
            .to_string(),
        serde_json::json!({
            "type": "attachment",
            "sessionId": SESSION,
            "attachment": { "content": "SECRET_ATTACHMENT" }
        })
        .to_string(),
        serde_json::json!({
            "type": "file-history-snapshot",
            "sessionId": SESSION,
            "snapshot": { "contents": "SECRET_SNAPSHOT" }
        })
        .to_string(),
    ];
    lines.extend(response_records(
        "msg_main",
        "2026-09-20T10:00:05.000Z",
        40,
        false,
    ));
    lines.extend(response_records(
        "msg_sub",
        "2026-09-20T10:00:06.000Z",
        7,
        true,
    ));
    lines.push(
        serde_json::json!({
            "type": "user",
            "sessionId": SESSION,
            "timestamp": "2026-09-20T10:00:07.000Z",
            "message": { "content": [
                { "type": "tool_result", "is_error": true, "content": "SECRET_TOOL_RESULT" }
            ]},
            "toolUseResult": { "stdout": "SECRET_STDOUT" }
        })
        .to_string(),
    );
    lines.push("{ this is not json".to_string());
    write_transcript(temp.path(), SESSION, &lines);
    temp
}

#[test]
fn transcript_ingestion_is_namespaced_private_and_keeps_safe_telemetry() {
    let temp = fixture();
    let snapshot = read_snapshot(temp.path());
    assert_eq!(snapshot.threads.len(), 1);
    let thread = &snapshot.threads[0];
    assert_eq!(thread.id, format!("claude:{SESSION}"));
    assert_eq!(thread.cwd.as_deref(), Some("/Users/someone/repo"));
    assert_eq!(thread.nickname.as_deref(), Some("Claude Code @ main"));
    assert_eq!(thread.role.as_deref(), Some("cli"));
    assert_eq!(thread.source_kind, "claude_code");

    // Sidechain (subagent) turns must not overwrite the session's own model.
    assert_eq!(thread.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(thread.effort.as_deref(), Some("high"));

    // Safe telemetry is kept: tool names, statuses, event kinds.
    assert!(thread
        .activity
        .iter()
        .any(|item| item.kind == "tool_use" && item.tool_name.as_deref() == Some("Bash")));
    assert!(thread
        .activity
        .iter()
        .any(|item| item.kind == "tool_result" && item.status.as_deref() == Some("error")));
    assert!(
        thread
            .activity
            .iter()
            .any(|item| item.kind == "assistant_message"
                && item.status.as_deref() == Some("end_turn"))
    );

    // Malformed lines are diagnosed, not fatal.
    assert!(thread
        .warnings
        .iter()
        .any(|warning| warning.contains("malformed")));

    let encoded = format!("{thread:?}");
    assert!(
        !encoded.contains("SECRET_"),
        "private content leaked: {encoded}"
    );
}

#[test]
fn duplicate_response_records_are_counted_once() {
    let temp = fixture();
    let snapshot = read_snapshot(temp.path());
    let usage = snapshot.threads[0].token_usage.as_ref().unwrap();
    // Two responses (main + sidechain), each written as two records. Counting
    // per record instead of per message id would double every counter.
    assert_eq!(usage.input_tokens, Some(20));
    assert_eq!(usage.cached_input_tokens, Some(200));
    assert_eq!(usage.cache_write_input_tokens, Some(40));
    assert_eq!(usage.output_tokens, Some(47));
    assert_eq!(usage.reasoning_output_tokens, Some(10));
    assert_eq!(usage.total_tokens, Some(307));
    // Claude Code does not persist a context window size.
    assert_eq!(usage.context_window, None);
}

#[test]
fn running_requires_a_live_registered_process() {
    let temp = fixture();
    let sessions = temp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    // A registry entry for a process that no longer exists is stale.
    fs::write(
        sessions.join("4294967294.json"),
        serde_json::json!({ "pid": 4_294_967_294_u32, "sessionId": SESSION, "cwd": "/Users/someone/repo" })
            .to_string(),
    )
    .unwrap();
    assert_eq!(
        read_snapshot(temp.path()).threads[0].state,
        ThreadState::Idle
    );

    // The test process itself is visible, so the session counts as running.
    let pid = std::process::id();
    fs::write(
        sessions.join(format!("{pid}.json")),
        serde_json::json!({ "pid": pid, "sessionId": SESSION, "entrypoint": "claude-desktop" })
            .to_string(),
    )
    .unwrap();
    assert_eq!(
        read_snapshot(temp.path()).threads[0].state,
        ThreadState::Running
    );
}

#[test]
fn missing_and_malformed_sources_are_non_fatal() {
    let temp = tempfile::tempdir().unwrap();
    let snapshot = read_snapshot(temp.path());
    assert!(snapshot.threads.is_empty());
    assert!(snapshot
        .warnings
        .iter()
        .any(|warning| warning.contains("projects directory missing")));

    let home = temp.path().join("home");
    write_transcript(&home, SESSION, &["".to_string()]);
    let sessions = home.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("1.json"), "not json").unwrap();
    let snapshot = read_snapshot(&home);
    assert!(snapshot
        .warnings
        .iter()
        .any(|warning| warning.contains("Malformed Claude Code session registry")));
    // An empty transcript still yields a row, with no state asserted.
    assert_eq!(snapshot.threads[0].state, ThreadState::Unknown);
}

#[test]
fn monitor_exposes_claude_provider_and_raw_thread_filter() {
    let temp = fixture();
    let mut monitor = Monitor::new_with_sources_and_usage(
        Some(temp.path().join("codex").display().to_string()),
        None,
        None,
        Provider::Claude,
        None,
        true,
    )
    .unwrap();
    monitor
        .set_claude_home(Some(temp.path().display().to_string()))
        .unwrap();

    let mut opts = filters(Provider::Claude);
    opts.thread = Some(SESSION.to_string());
    let output = monitor
        .probe_snapshot(&opts, Default::default(), true)
        .unwrap();

    assert_eq!(output.query.provider.as_deref(), Some("claude"));
    assert_eq!(output.threads.len(), 1);
    let thread = &output.threads[0];
    assert_eq!(thread.thread_id, format!("claude:{SESSION}"));
    assert_eq!(thread.source_kind.as_deref(), Some("claude_code"));
    // Claude sessions are independent roots: no fabricated parent edges.
    assert!(thread.parent_thread_id.is_none());
    assert_eq!(
        thread
            .model
            .requested
            .value
            .as_ref()
            .and_then(|spec| spec.model.as_deref()),
        Some("claude-opus-5")
    );
    assert_eq!(
        thread
            .token_usage
            .value
            .as_ref()
            .and_then(|u| u.total_tokens),
        Some(307)
    );
    let encoded = serde_json::to_string(&output).unwrap();
    assert!(!encoded.contains("SECRET_"), "private content leaked");
}

#[test]
fn plan_usage_reports_newest_sample_without_the_account_identifier() {
    let history = serde_json::json!({
        "version": 2,
        "samples": [
            // No percentages yet: must not be chosen over a later real sample.
            { "t": 1_789_000_000_000_u64, "org": "SECRET_ORG", "u": {} },
            { "t": 1_789_900_000_000_u64, "org": "SECRET_ORG", "u": { "fh": 13, "sd": 2 } },
            { "t": 1_789_923_790_763_u64, "org": "SECRET_ORG", "u": { "fh": 20, "sd": 3 } },
        ]
    });
    let observed = parse_plan_usage(&history);
    let usage = observed.value.as_ref().expect("plan usage");

    let five_hour = usage.primary.as_ref().expect("five hour window");
    assert_eq!(five_hour.used_percent, Some(20.0));
    assert_eq!(five_hour.window_minutes, Some(300));
    // The history records utilisation only; no reset time may be invented.
    assert_eq!(five_hour.resets_at, None);

    let weekly = usage.secondary.as_ref().expect("seven day window");
    assert_eq!(weekly.used_percent, Some(3.0));
    assert_eq!(weekly.window_minutes, Some(10_080));
    assert_eq!(weekly.resets_at, None);

    assert_eq!(
        observed.observed_at.map(|at| at.timestamp_millis()),
        Some(1_789_923_790_763)
    );

    let encoded = serde_json::to_string(&observed).unwrap();
    assert!(
        !encoded.contains("SECRET_ORG"),
        "account id leaked: {encoded}"
    );
}

#[test]
fn plan_usage_rejects_missing_malformed_and_out_of_range_evidence() {
    // Absent file.
    let temp = tempfile::tempdir().unwrap();
    assert!(read_plan_usage(Some(&temp.path().join("nope.json")))
        .value
        .is_none());

    // Malformed file.
    let bad = temp.path().join("bad.json");
    fs::write(&bad, "not json").unwrap();
    assert!(read_plan_usage(Some(&bad)).value.is_none());

    // Right shape, no usable samples.
    assert!(parse_plan_usage(&serde_json::json!({ "samples": [] }))
        .value
        .is_none());
    assert!(parse_plan_usage(&serde_json::json!({ "version": 2 }))
        .value
        .is_none());

    // Impossible percentages are discarded rather than reported.
    let out_of_range = parse_plan_usage(&serde_json::json!({
        "samples": [{ "t": 1_789_923_790_763_u64, "u": { "fh": 140, "sd": -5 } }]
    }));
    assert!(out_of_range.value.is_none());

    // A partial sample still reports the window it does have.
    let partial = parse_plan_usage(&serde_json::json!({
        "samples": [{ "t": 1_789_923_790_763_u64, "u": { "fh": 42 } }]
    }));
    let usage = partial.value.as_ref().expect("partial plan usage");
    assert_eq!(
        usage.primary.as_ref().and_then(|w| w.used_percent),
        Some(42.0)
    );
    assert!(usage.secondary.is_none());
}

#[test]
fn plan_usage_reaches_the_probe_output() {
    let temp = fixture();
    let history = temp.path().join("plan-usage-history.json");
    fs::write(
        &history,
        serde_json::json!({
            "samples": [{ "t": 1_789_923_790_763_u64, "u": { "fh": 20, "sd": 3 } }]
        })
        .to_string(),
    )
    .unwrap();

    let mut monitor = Monitor::new_with_sources_and_usage(
        Some(temp.path().join("codex").display().to_string()),
        None,
        None,
        Provider::Claude,
        None,
        true,
    )
    .unwrap();
    monitor
        .set_claude_home(Some(temp.path().display().to_string()))
        .unwrap();
    monitor.set_claude_usage_file(Some(history.display().to_string()));

    let output = monitor
        .probe_snapshot(&filters(Provider::Claude), Default::default(), true)
        .unwrap();
    let usage = output
        .claude_account_usage
        .value
        .as_ref()
        .expect("claude account usage");
    assert_eq!(
        usage.primary.as_ref().and_then(|w| w.used_percent),
        Some(20.0)
    );
    assert_eq!(
        usage.secondary.as_ref().and_then(|w| w.window_minutes),
        Some(10_080)
    );
}
