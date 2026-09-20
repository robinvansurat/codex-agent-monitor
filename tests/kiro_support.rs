use std::fs;
use std::path::Path;

use chrono::{Duration, Utc};
use codex_agent_monitor::cli::{FilterOpts, OutputFormat, Provider, ThreadStateFilter};
use codex_agent_monitor::kiro::read_snapshot;
use codex_agent_monitor::model::{TaskStatus, ThreadState};
use codex_agent_monitor::observer::Monitor;
use rusqlite::Connection;
use tempfile::TempDir;

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn fixture() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    write(
        &home.join("sessions/cli/native-1.json"),
        r#"{
          "session_id":"native-1","cwd":"/work/native","created_at":"2026-09-12T10:00:00Z","updated_at":"2026-09-12T10:02:00Z",
          "session_state":{"agent_name":"native-agent","rts_model_state":{"model_info":{"model_id":"kiro-model"},"additional_fields":{"overrides":{"output_config":{"effort":"high"}}}},"conversation_metadata":{"user_turn_metadatas":[{"end_reason":"UserTurnEnd","end_timestamp":"2026-09-12T10:01:00Z","result":{"Ok":{"private":"DO NOT RETAIN"}}}]}}
        }"#,
    );
    write(
        &home.join("sessions/cli/native-1.jsonl"),
        "{\"version\":1,\"kind\":\"Prompt\",\"data\":{\"meta\":{\"timestamp\":1789210800},\"content\":[{\"text\":\"SECRET_PROMPT\"}]}}\n{\"version\":1,\"kind\":\"AssistantMessage\",\"data\":{\"meta\":{\"timestamp\":1789210860},\"content\":[{\"text\":\"SECRET_RESPONSE\"}]}}\n",
    );
    write(
        &home.join("sessions/workspace/sess_acp-1/session.json"),
        r#"{"schemaVersion":"1.0.0","dataModelVersion":1,"id":"sess_acp-1","agentMode":"codex-kiro-worker","workspacePaths":["/work/acp"],"rootPaths":[],"createdAt":"2026-09-12T09:00:00Z","lastModifiedAt":"2026-09-12T09:03:00Z","modelId":"acp-model","effortLevel":"medium","status":"idle"}"#,
    );
    write(
        &home.join("sessions/workspace/sess_acp-1/messages.jsonl"),
        "{\"timestamp\":\"2026-09-12T09:02:00Z\",\"payload\":{\"type\":\"turn_end\",\"stopReason\":\"end_turn\",\"secret\":\"NO\"}}\n{\"timestamp\":\"2026-09-12T09:01:00Z\",\"payload\":{\"type\":\"turn_start\"}}\n{\"timestamp\":\"2026-09-12T09:01:30Z\",\"payload\":{\"type\":\"tool_call\",\"toolName\":\"read_file\",\"status\":\"completed\",\"arguments\":\"SECRET_ARGS\"}}\n",
    );
    let db_path = home.join("data.sqlite3");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE conversations_v2 (key TEXT, conversation_id TEXT, value TEXT, created_at INTEGER, updated_at INTEGER); CREATE TABLE conversations (key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    let duplicate =
        r#"{"conversation_id":"native-1","model_info":{"model_id":"legacy-model"},"history":[]}"#;
    let classic = r#"{"conversation_id":"classic-1","model_info":{"model_id":"classic-model"},"history":[{"user":{"timestamp":"2026-09-12T08:00:00Z","content":"SECRET_HISTORY"}},{"assistant":{"Response":{"text":"SECRET_RESPONSE"}}}]}"#;
    conn.execute(
        "INSERT INTO conversations_v2 (key, conversation_id, value, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        ("/work/native", "native-1", duplicate, 1_000_i64, 2_000_i64),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conversations (key, value) VALUES (?1, ?2)",
        ("C:\\work\\classic", classic),
    )
    .unwrap();
    temp
}

fn filters(provider: Provider) -> FilterOpts {
    FilterOpts {
        provider,
        kiro_home: None,
        kiro_db: None,
        kiro_cli: None,
        no_kiro_usage: true,
        all: true,
        project: None,
        thread: None,
        state: None,
        recent_minutes: None,
        depth: None,
        role: None,
        runtime_events: None,
        format: OutputFormat::Json,
        interval_ms: None,
    }
}

fn write_acp_session(home: &Path, status: &str, modified: Option<String>, messages: &str) {
    let session_dir = home.join("sessions/ws/sess_fixture");
    let modified = modified.unwrap_or_default();
    write(
        &session_dir.join("session.json"),
        &format!(
            r#"{{"schemaVersion":"1.0.0","dataModelVersion":1,"id":"sess_fixture","workspacePaths":["/work/lifecycle"],"rootPaths":[],"createdAt":"2026-09-12T09:00:00Z","lastModifiedAt":{},"status":"{}","agentMode":"fixture","modelId":"model"}}"#,
            if modified.is_empty() {
                "null".to_string()
            } else {
                format!("\"{modified}\"")
            },
            status,
        ),
    );
    write(&session_dir.join("messages.jsonl"), messages);
}

fn probe_fixture(home: &Path) -> codex_agent_monitor::model::ThreadSnapshot {
    let mut monitor = Monitor::new_with_sources(
        Some(home.join("codex").display().to_string()),
        Some(home.display().to_string()),
        None,
        Provider::Kiro,
    )
    .unwrap();
    let output = monitor
        .probe_snapshot(&filters(Provider::Kiro), Default::default(), true)
        .unwrap();
    assert_eq!(output.threads.len(), 1, "{output:?}");
    output.threads.into_iter().next().unwrap()
}

#[test]
fn reads_native_acp_and_classic_without_private_content() {
    let temp = fixture();
    let before = fs::read(temp.path().join("data.sqlite3")).unwrap();
    let snapshot = read_snapshot(temp.path(), Some(&temp.path().join("data.sqlite3")), true);
    let after = fs::read(temp.path().join("data.sqlite3")).unwrap();
    assert_eq!(before, after);
    assert_eq!(snapshot.threads.len(), 3);
    let native = snapshot
        .threads
        .iter()
        .find(|v| v.id == "kiro:native-1")
        .unwrap();
    assert_eq!(native.source_kind, "kiro_cli");
    assert_eq!(native.cwd.as_deref(), Some("/work/native"));
    assert_eq!(native.model.as_deref(), Some("kiro-model"));
    assert!(snapshot
        .threads
        .iter()
        .any(|v| v.id == "kiro:sess_acp-1" && v.source_kind == "kiro_acp"));
    assert!(snapshot.threads.iter().any(|v| v.id == "kiro:classic-1"));
    let mut monitor = Monitor::new_with_sources(
        Some(temp.path().join("codex").display().to_string()),
        Some(temp.path().display().to_string()),
        Some(temp.path().join("data.sqlite3").display().to_string()),
        Provider::Kiro,
    )
    .unwrap();
    let encoded = serde_json::to_string(
        &monitor
            .probe_snapshot(&filters(Provider::Kiro), Default::default(), true)
            .unwrap(),
    )
    .unwrap();
    assert!(!encoded.contains("SECRET_"));
    assert!(!encoded.contains("legacy-model"));
}

#[test]
fn kiro_provider_isolated_from_broken_codex_config_and_supports_raw_ids_and_filters() {
    let temp = fixture();
    let codex = tempfile::tempdir().unwrap();
    write(
        &codex.path().join("config.toml"),
        "this is not valid toml = [",
    );
    let mut monitor = Monitor::new_with_sources(
        Some(codex.path().display().to_string()),
        Some(temp.path().display().to_string()),
        Some(temp.path().join("data.sqlite3").display().to_string()),
        Provider::Kiro,
    )
    .unwrap();
    let mut opts = filters(Provider::Kiro);
    opts.kiro_home = Some(temp.path().display().to_string());
    opts.thread = Some("native-1".to_string());
    let output = monitor
        .probe_snapshot(&opts, Default::default(), true)
        .unwrap();
    assert_eq!(output.threads.len(), 1);
    assert_eq!(output.threads[0].thread_id, "kiro:native-1");
    assert_eq!(output.threads[0].source_kind.as_deref(), Some("kiro_cli"));
    assert_eq!(output.threads[0].state, ThreadState::Unknown);

    opts.thread = None;
    opts.project = Some("/work/acp".to_string());
    let output = monitor
        .probe_snapshot(&opts, Default::default(), true)
        .unwrap();
    assert_eq!(output.threads.len(), 1);
    assert_eq!(output.threads[0].thread_id, "kiro:sess_acp-1");
    assert_eq!(output.threads[0].state, ThreadState::Idle);

    opts.project = None;
    opts.state = Some(ThreadStateFilter::Idle);
    opts.role = Some("codex-kiro-worker".to_string());
    let output = monitor
        .probe_snapshot(&opts, Default::default(), true)
        .unwrap();
    assert_eq!(output.threads.len(), 1);
    assert_eq!(output.threads[0].thread_id, "kiro:sess_acp-1");
}

#[test]
fn malformed_and_unsupported_sessions_are_warned_and_skipped() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/bad.json"),
        "{\"title\":\"SECRET_TITLE\"}",
    );
    write(
        &temp.path().join("sessions/ws/sess-bad/session.json"),
        "{\"id\":\"sess-bad\",\"data\":{}}",
    );
    let snapshot = read_snapshot(temp.path(), None, true);
    assert!(snapshot.threads.is_empty());
    let warnings = snapshot.warnings.join(" ");
    assert!(warnings.contains("unsupported Kiro") || warnings.contains("no id"));
    assert!(!warnings.contains("SECRET_TITLE"));
}

#[test]
fn acp_lifecycle_metadata_and_dated_events_are_merged_conservatively() {
    let temp = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let t1 = (now - Duration::seconds(40)).to_rfc3339();
    let t2 = (now - Duration::seconds(20)).to_rfc3339();
    let t3 = (now - Duration::seconds(10)).to_rfc3339();
    let turn_start = |timestamp: &str| {
        format!("{{\"timestamp\":\"{timestamp}\",\"payload\":{{\"type\":\"turn_start\"}}}}\n")
    };
    let completed = |timestamp: &str| {
        format!(
            "{{\"timestamp\":\"{timestamp}\",\"payload\":{{\"type\":\"turn_end\",\"stopReason\":\"end_turn\"}}}}\n"
        )
    };
    let unknown_end = |timestamp: &str| {
        format!(
            "{{\"timestamp\":\"{timestamp}\",\"payload\":{{\"type\":\"turn_end\",\"stopReason\":\"future_reason\"}}}}\n"
        )
    };

    write_acp_session(temp.path(), "idle", Some(t1.clone()), &turn_start(&t2));
    let snapshot = probe_fixture(temp.path());
    assert_eq!(snapshot.state, ThreadState::Running);
    assert_eq!(
        snapshot.evidence.state.observed_at.unwrap().to_rfc3339(),
        t2
    );
    assert_eq!(
        snapshot.evidence.state.confidence,
        codex_agent_monitor::model::Confidence::Medium
    );

    write_acp_session(temp.path(), "failed", Some(t3.clone()), &completed(&t2));
    let snapshot = probe_fixture(temp.path());
    assert_eq!(snapshot.state, ThreadState::Idle);
    assert_eq!(
        snapshot.last_terminal_event.value,
        Some(codex_agent_monitor::model::LastTerminalEvent::Failed)
    );
    assert_eq!(
        snapshot
            .last_terminal_event
            .observed_at
            .unwrap()
            .to_rfc3339(),
        t3
    );
    assert_eq!(
        snapshot.evidence.state.observed_at.unwrap().to_rfc3339(),
        t3
    );

    write_acp_session(temp.path(), "idle", Some(t3.clone()), &turn_start(&t2));
    let snapshot = probe_fixture(temp.path());
    assert_eq!(snapshot.state, ThreadState::Idle);
    assert!(snapshot.last_terminal_event.value.is_none());
    assert_eq!(
        snapshot.evidence.state.observed_at.unwrap().to_rfc3339(),
        t3
    );

    write_acp_session(temp.path(), "failed", Some(t1.clone()), &turn_start(&t2));
    let snapshot = probe_fixture(temp.path());
    assert_eq!(snapshot.state, ThreadState::Running);
    assert_eq!(
        snapshot.last_terminal_event.value,
        Some(codex_agent_monitor::model::LastTerminalEvent::Failed)
    );
    assert_eq!(
        snapshot
            .last_terminal_event
            .observed_at
            .unwrap()
            .to_rfc3339(),
        t1
    );

    write_acp_session(
        temp.path(),
        "idle",
        Some(t1.clone()),
        &format!("{}{}", turn_start(&t2), unknown_end(&t3)),
    );
    let snapshot = probe_fixture(temp.path());
    assert_eq!(snapshot.state, ThreadState::Unknown);
    assert!(snapshot.last_terminal_event.value.is_none());

    write_acp_session(
        temp.path(),
        "running",
        Some(t1.clone()),
        "{\"payload\":{\"type\":\"turn_start\"}}\n",
    );
    let snapshot = probe_fixture(temp.path());
    assert_eq!(snapshot.state, ThreadState::Running);
    assert_eq!(
        snapshot.evidence.state.confidence,
        codex_agent_monitor::model::Confidence::Low
    );
    assert!(snapshot.evidence.state.observed_at.is_none());

    write_acp_session(
        temp.path(),
        "running",
        Some(t1.clone()),
        &format!("{{malformed}}\n{}", turn_start(&t2)),
    );
    let parsed = read_snapshot(temp.path(), None, true);
    assert!(parsed
        .warnings
        .iter()
        .any(|warning| warning.contains("malformed Kiro ACP event")));
    let snapshot = probe_fixture(temp.path());
    assert_eq!(snapshot.state, ThreadState::Running);

    write_acp_session(temp.path(), "running", Some(t1), "{malformed}\n");
    let snapshot = probe_fixture(temp.path());
    assert_eq!(snapshot.state, ThreadState::Unknown);
}

#[test]
fn runtime_overlay_is_ignored_for_kiro_rows() {
    let temp = fixture();
    let mut monitor = Monitor::new_with_sources(
        Some(temp.path().join("codex").display().to_string()),
        Some(temp.path().display().to_string()),
        Some(temp.path().join("data.sqlite3").display().to_string()),
        Provider::Kiro,
    )
    .unwrap();
    let mut opts = filters(Provider::Kiro);
    opts.thread = Some("native-1".to_string());
    let mut runtime = codex_agent_monitor::runtime::RuntimeOverlay::default();
    runtime.effective_model.insert(
        "kiro:native-1".to_string(),
        codex_agent_monitor::model::Observed::from_value(
            Some(codex_agent_monitor::model::ModelSpec {
                model: Some("spoofed".to_string()),
                reasoning_effort: None,
            }),
            "test",
            None,
            None,
        ),
    );
    runtime.rerouted_from.insert(
        "kiro:native-1".to_string(),
        codex_agent_monitor::model::Observed::from_value(
            Some("original".to_string()),
            "test",
            None,
            None,
        ),
    );
    runtime.reroute_reason.insert(
        "kiro:native-1".to_string(),
        codex_agent_monitor::model::Observed::from_value(
            Some("reason".to_string()),
            "test",
            None,
            None,
        ),
    );
    let snapshot = monitor.probe_snapshot(&opts, runtime, true).unwrap();
    assert!(snapshot.threads[0].model.effective.value.is_none());
    assert!(snapshot.threads[0].model.rerouted_from.is_none());
    assert!(snapshot.threads[0].model.reroute_reason.is_none());
}

#[cfg(unix)]
#[test]
fn symlinked_kiro_session_roots_are_skipped() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("real/sessions/cli")).unwrap();
    write(
        &temp.path().join("real/sessions/cli/native.json"),
        r#"{"session_id":"behind-link","cwd":"/work/link","session_state":{}}"#,
    );
    std::os::unix::fs::symlink(
        temp.path().join("real/sessions"),
        temp.path().join("sessions"),
    )
    .unwrap();
    let snapshot = read_snapshot(temp.path(), None, true);
    assert!(snapshot.threads.is_empty());
    assert!(snapshot
        .warnings
        .iter()
        .any(|warning| warning.contains("symlinked")));
}

#[test]
fn native_task_progress_exposes_only_ids_and_statuses() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/task-session.json"),
        r#"{
          "session_id":"task-session","cwd":"/work/tasks","created_at":"2026-09-12T10:00:00Z","updated_at":"2026-09-12T10:02:00Z","title":"SECRET_TITLE",
          "session_state":{"agent_name":"default"}
        }"#,
    );
    write(
        &temp.path().join("sessions/cli/task-session/tasks/1.json"),
        r#"{"id":"1","status":"in_progress","subject":"SECRET_SUBJECT","description":"SECRET_DESCRIPTION"}"#,
    );
    write(
        &temp.path().join("sessions/cli/task-session/tasks/2.json"),
        r#"{"id":"2","status":"completed","subject":"SECRET_SECOND_SUBJECT","description":"SECRET_SECOND_DESCRIPTION"}"#,
    );
    write(
        &temp
            .path()
            .join("sessions/cli/task-session/tasks/project_metadata.json"),
        r#"{"description":"SECRET_PROJECT_DESCRIPTION"}"#,
    );

    let mut monitor = Monitor::new_with_sources(
        Some(temp.path().join("codex").display().to_string()),
        Some(temp.path().display().to_string()),
        None,
        Provider::Kiro,
    )
    .unwrap();
    let output = monitor
        .probe_snapshot(&filters(Provider::Kiro), Default::default(), true)
        .unwrap();
    let thread = output.threads.first().unwrap();
    let progress = thread.task_progress.value.as_ref().unwrap();
    assert_eq!(progress.tasks.len(), 2);
    assert_eq!(progress.tasks[0].id, 1);
    assert_eq!(progress.tasks[0].status, TaskStatus::InProgress);
    assert_eq!(progress.tasks[1].id, 2);
    assert_eq!(progress.tasks[1].status, TaskStatus::Completed);
    assert_eq!(
        thread
            .task_progress
            .source
            .as_ref()
            .map(|source| source.kind.as_str()),
        Some("kiro.tasks")
    );

    let encoded = serde_json::to_string(&output).unwrap();
    let human = codex_agent_monitor::output::render_human(&output);
    assert!(human.contains("task progress: 1/2 completed"));
    assert!(human.contains("#1 in progress"));
    assert!(human.contains("#2 completed"));
    for secret in [
        "SECRET_TITLE",
        "SECRET_SUBJECT",
        "SECRET_DESCRIPTION",
        "SECRET_SECOND_SUBJECT",
        "SECRET_SECOND_DESCRIPTION",
        "SECRET_PROJECT_DESCRIPTION",
    ] {
        assert!(!encoded.contains(secret));
        assert!(!human.contains(secret));
    }
}

#[test]
fn native_tool_activity_keeps_names_statuses_and_stream_order_only() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/tool-session.json"),
        r#"{
          "session_id":"tool-session","cwd":"/work/tools","created_at":"2026-09-12T10:00:00Z","updated_at":"2026-09-12T10:02:00Z",
          "session_state":{"agent_name":"default"}
        }"#,
    );
    write(
        &temp.path().join("sessions/cli/tool-session.jsonl"),
        concat!(
            r#"{"version":1,"kind":"Prompt","data":{"meta":{"timestamp":1789210800},"content":[{"kind":"text","data":"SECRET_PROMPT"}]}}"#,
            "\n",
            r#"{"version":1,"kind":"AssistantMessage","data":{"content":[{"kind":"toolUse","data":{"toolUseId":"call-1","name":"read","input":{"path":"SECRET_INPUT"}}}]}}"#,
            "\n",
            r#"{"version":1,"kind":"ToolResults","data":{"content":[{"kind":"toolResult","data":{"toolUseId":"call-1","content":"SECRET_RESULT","status":"success"}}],"results":{"call-1":{"tool":{"kind":"read","tool_use_purpose":"SECRET_PURPOSE"},"result":{"Success":{"content":"SECRET_NESTED_RESULT"}}}}}}"#,
            "\n",
        ),
    );

    let mut monitor = Monitor::new_with_sources(
        Some(temp.path().join("codex").display().to_string()),
        Some(temp.path().display().to_string()),
        None,
        Provider::Kiro,
    )
    .unwrap();
    let output = monitor
        .probe_snapshot(&filters(Provider::Kiro), Default::default(), true)
        .unwrap();
    let activity = &output.threads[0].recent_activity;
    let safe_details = activity
        .iter()
        .map(|event| {
            (
                event.kind.as_str(),
                event.tool_name.as_deref(),
                event.status.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        safe_details,
        vec![
            ("prompt", None, None),
            ("tool_call", Some("read"), None),
            ("tool_result", Some("read"), Some("success")),
        ]
    );

    let encoded = serde_json::to_string(&output).unwrap();
    for secret in [
        "SECRET_PROMPT",
        "SECRET_INPUT",
        "SECRET_RESULT",
        "SECRET_PURPOSE",
        "SECRET_NESTED_RESULT",
        "call-1",
    ] {
        assert!(!encoded.contains(secret));
    }
}

#[test]
fn native_event_reader_skips_large_ignored_payloads() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/large-payload.json"),
        r#"{"session_id":"large-payload","cwd":"/work/large","session_state":{"agent_name":"default"}}"#,
    );
    let large_array = format!("{}0", "0,".repeat(2_000_000));
    write(
        &temp.path().join("sessions/cli/large-payload.jsonl"),
        &format!(
            "{{\"version\":1,\"kind\":\"Prompt\",\"data\":{{\"meta\":{{\"timestamp\":1789210800}},\"content\":[{{\"kind\":\"text\",\"data\":{{\"source\":{{\"data\":[{}]}}}}}}]}}}}\n",
            large_array
        ),
    );

    let snapshot = probe_fixture(temp.path());
    let activity = &snapshot.recent_activity;
    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].kind, "prompt");
    assert_eq!(
        activity[0].timestamp,
        Some(
            "2026-09-12T11:00:00Z"
                .parse::<chrono::DateTime<Utc>>()
                .unwrap()
        )
    );
}

#[test]
fn task_store_without_valid_tasks_is_not_reported() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/empty-tasks.json"),
        r#"{"session_id":"empty-tasks","cwd":"/work/tasks","updated_at":"2026-09-12T10:02:00Z","session_state":{"agent_name":"default"}}"#,
    );
    write(
        &temp
            .path()
            .join("sessions/cli/empty-tasks/tasks/project_metadata.json"),
        r#"{"description":"SECRET_PROJECT_DESCRIPTION"}"#,
    );
    write(
        &temp.path().join("sessions/cli/empty-tasks/tasks/1.json"),
        r#"{"id":"2","status":"completed","subject":"SECRET_MISMATCHED_SUBJECT"}"#,
    );

    let mut monitor = Monitor::new_with_sources(
        Some(temp.path().join("codex").display().to_string()),
        Some(temp.path().display().to_string()),
        None,
        Provider::Kiro,
    )
    .unwrap();
    let output = monitor
        .probe_snapshot(&filters(Provider::Kiro), Default::default(), true)
        .unwrap();
    assert!(output.threads[0].task_progress.value.is_none());
    assert!(output
        .warnings
        .iter()
        .any(|warning| warning.contains("task id mismatch")));
    let human = codex_agent_monitor::output::render_human(&output);
    assert!(!human.contains("task progress: 0/0"));
    assert!(!serde_json::to_string(&output).unwrap().contains("SECRET_"));
}

#[test]
fn kiro_task_mtime_uses_persisted_state_activity_provenance() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/task-recency.json"),
        r#"{"session_id":"task-recency","cwd":"/work/tasks","created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-01T00:00:00Z","session_state":{"agent_name":"default"}}"#,
    );
    write(
        &temp.path().join("sessions/cli/task-recency/tasks/1.json"),
        r#"{"id":"1","status":"pending","subject":"SECRET_SUBJECT"}"#,
    );

    let snapshot = probe_fixture(temp.path());
    assert_eq!(
        snapshot
            .activity_signal
            .source
            .as_ref()
            .map(|source| source.kind.as_str()),
        Some("kiro.persisted-state")
    );
    assert_eq!(
        snapshot.activity_signal.confidence,
        codex_agent_monitor::model::Confidence::Low
    );
}

#[test]
fn native_pending_turn_is_running_only_with_a_live_session_lock() {
    let temp = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let completed_at = (now - Duration::seconds(30)).to_rfc3339();
    let prompt_at = (now - Duration::seconds(10)).timestamp();
    let finished_at = now.to_rfc3339();
    let session_path = temp.path().join("sessions/cli/live-turn.json");
    let event_path = temp.path().join("sessions/cli/live-turn.jsonl");
    let lock_path = temp.path().join("sessions/cli/live-turn.lock");
    let session = |end_timestamp: &str| {
        format!(
            r#"{{"session_id":"live-turn","cwd":"/work/live","updated_at":"{end_timestamp}","session_state":{{"agent_name":"default","conversation_metadata":{{"user_turn_metadatas":[{{"end_reason":"UserTurnEnd","end_timestamp":"{end_timestamp}","result":{{"Ok":{{}}}}}}]}}}}}}"#
        )
    };
    write(&session_path, &session(&completed_at));
    write(
        &event_path,
        &format!(
            "{{\"version\":1,\"kind\":\"Prompt\",\"data\":{{\"meta\":{{\"timestamp\":{prompt_at}}},\"content\":[{{\"kind\":\"text\",\"data\":\"SECRET_PROMPT\"}}]}}}}\n"
        ),
    );

    let without_lock = probe_fixture(temp.path());
    assert_eq!(without_lock.state, ThreadState::Unknown);

    write(
        &lock_path,
        &format!(r#"{{"pid":0,"started_at":"{completed_at}"}}"#),
    );
    let dead_lock = probe_fixture(temp.path());
    assert_eq!(dead_lock.state, ThreadState::Unknown);

    write(
        &lock_path,
        &format!(
            r#"{{"pid":{},"started_at":"{completed_at}"}}"#,
            std::process::id()
        ),
    );
    let running = probe_fixture(temp.path());
    assert_eq!(running.state, ThreadState::Running);
    assert_eq!(
        running.evidence.state.observed_at.unwrap().timestamp(),
        prompt_at
    );
    assert_eq!(
        running.evidence.state.confidence,
        codex_agent_monitor::model::Confidence::Medium
    );

    write(&session_path, &session(&finished_at));
    let idle = probe_fixture(temp.path());
    assert_eq!(idle.state, ThreadState::Idle);
    assert_eq!(
        idle.last_terminal_event.value,
        Some(codex_agent_monitor::model::LastTerminalEvent::Completed)
    );
}

#[test]
fn native_context_usage_is_separate_from_tokens_and_missing_task_plans_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/context-session.json"),
        r#"{
          "session_id":"context-session","cwd":"/work/context","created_at":"2026-09-12T10:00:00Z","updated_at":"2026-09-12T10:02:00Z","title":"SECRET_TITLE",
          "session_state":{
            "agent_name":"default",
            "rts_model_state":{
              "model_info":{"model_id":"kiro-model","context_window_tokens":272000},
              "context_usage_percentage":94.69118
            },
            "conversation_metadata":{
              "last_context_usage":{"percentage":94.69118,"model_id":"kiro-model"},
              "user_turn_metadatas":[{
                "end_reason":"UserTurnEnd","end_timestamp":"2026-09-12T10:01:00Z",
                "input_token_count":0,"cache_read_input_token_count":0,
                "cache_write_input_token_count":0,"output_token_count":0,
                "metering_usage":[{"unit":"credit","unitPlural":"credits","value":2.1543443526}],
                "result":{"Ok":{"private":"SECRET_RESULT"}}
              }]
            }
          }
        }"#,
    );

    let mut monitor = Monitor::new_with_sources(
        Some(temp.path().join("codex").display().to_string()),
        Some(temp.path().display().to_string()),
        None,
        Provider::Kiro,
    )
    .unwrap();
    let output = monitor
        .probe_snapshot(&filters(Provider::Kiro), Default::default(), true)
        .unwrap();
    assert_eq!(output.threads.len(), 1);
    let thread = &output.threads[0];
    assert_eq!(thread.state, ThreadState::Idle);
    assert!(thread.token_usage.value.is_none());
    let context = thread.context_usage.value.as_ref().expect("context usage");
    assert!((context.used_percent - 94.69118).abs() < f64::EPSILON);
    assert_eq!(context.context_window_tokens, 272_000);
    assert_eq!(context.used_tokens_approx, 257_560);
    assert_eq!(
        thread
            .context_usage
            .source
            .as_ref()
            .map(|source| source.kind.as_str()),
        Some("kiro.session.context_usage")
    );
    assert_eq!(
        thread.context_usage.confidence,
        codex_agent_monitor::model::Confidence::Medium
    );
    assert!(thread
        .context_usage
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("not cumulative")));

    let encoded = serde_json::to_string(&output).unwrap();
    assert!(encoded.contains("\"context_usage\""));
    assert!(encoded.contains("\"used_percent\":94.69118"));
    assert!(encoded.contains("\"used_tokens_approx\":257560"));
    assert!(encoded.contains("\"context_window_tokens\":272000"));
    let human = codex_agent_monitor::output::render_human(&output);
    assert!(human.contains("context usage: 94.7% used (approximately 257,560 / 272,000 tokens)"));
    assert!(human.contains("task progress: unavailable (Kiro did not persist a task plan)"));
    for private in ["SECRET_TITLE", "SECRET_RESULT", "2.1543443526"] {
        assert!(!encoded.contains(private));
        assert!(!human.contains(private));
    }

    let mut running = filters(Provider::Kiro);
    running.state = Some(ThreadStateFilter::Running);
    let filtered = monitor
        .probe_snapshot(&running, Default::default(), true)
        .unwrap();
    assert!(filtered.threads.is_empty());
}

#[test]
fn malformed_or_incomplete_native_context_evidence_remains_unknown() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("sessions/cli/malformed-context.json"),
        r#"{
          "session_id":"malformed-context","cwd":"/work/context","updated_at":"2026-09-12T10:02:00Z",
          "session_state":{"agent_name":"default","rts_model_state":{
            "model_info":{"context_window_tokens":272000},
            "context_usage_percentage":"SECRET_PERCENT"
          },"conversation_metadata":{"last_context_usage":{"percentage":101.0}}}
        }"#,
    );
    write(
        &temp.path().join("sessions/cli/incomplete-context.json"),
        r#"{
          "session_id":"incomplete-context","cwd":"/work/context","updated_at":"2026-09-12T10:02:00Z",
          "session_state":{"agent_name":"default","rts_model_state":{
            "model_info":{"context_window_tokens":"SECRET_WINDOW"},
            "context_usage_percentage":50.0
          }}
        }"#,
    );

    let mut monitor = Monitor::new_with_sources(
        Some(temp.path().join("codex").display().to_string()),
        Some(temp.path().display().to_string()),
        None,
        Provider::Kiro,
    )
    .unwrap();
    let output = monitor
        .probe_snapshot(&filters(Provider::Kiro), Default::default(), true)
        .unwrap();
    assert_eq!(output.threads.len(), 2);
    assert!(output
        .threads
        .iter()
        .all(|thread| thread.context_usage.value.is_none()));
    let warnings = output.warnings.join(" ");
    assert!(warnings.contains("malformed Kiro native context usage"));
    assert!(warnings.contains("incomplete Kiro native context usage"));
    let encoded = serde_json::to_string(&output).unwrap();
    for private in ["SECRET_PERCENT", "SECRET_WINDOW"] {
        assert!(!warnings.contains(private));
        assert!(!encoded.contains(private));
    }
}
