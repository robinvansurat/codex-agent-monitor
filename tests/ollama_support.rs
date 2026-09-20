use std::fs;
use std::path::Path;

use chrono::{TimeZone, Utc};
use codex_agent_monitor::cli::{FilterOpts, OutputFormat, Provider};
use codex_agent_monitor::observer::Monitor;
use codex_agent_monitor::ollama::{
    parse_cli_processes, parse_ps_response, poll_server_at, read_snapshot,
};
use rusqlite::Connection;
use tempfile::TempDir;

fn filters(provider: Provider) -> FilterOpts {
    FilterOpts {
        provider,
        all: true,
        format: OutputFormat::Json,
        ..Default::default()
    }
}

fn fixture() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE chats (id TEXT PRIMARY KEY, title TEXT, created_at TEXT, browser_state TEXT); CREATE TABLE messages (id TEXT PRIMARY KEY, chat_id TEXT, role TEXT, content TEXT, thinking TEXT, stream INTEGER, model_name TEXT, created_at TEXT, updated_at TEXT, thinking_time_start TEXT, thinking_time_end TEXT, tool_result TEXT); CREATE TABLE tool_calls (id TEXT, message_id TEXT, name TEXT, arguments TEXT, result TEXT); INSERT INTO chats VALUES ('chat-1','SECRET_TITLE','2026-08-18 22:20:58.232713+07:00','SECRET_BROWSER'); INSERT INTO chats VALUES ('empty','SECRET_EMPTY','2026-08-18 22:20:58.232713+07:00','SECRET_BROWSER'); INSERT INTO messages VALUES ('msg-1','chat-1','user','SECRET_CONTENT','SECRET_THINKING',0,'llama3.2','2026-08-18 22:21:00.232713+07:00','2026-08-18 22:21:02.232713+07:00',NULL,NULL,'SECRET_TOOL_RESULT'); INSERT INTO tool_calls VALUES ('tool-1','msg-1','SECRET_TOOL','SECRET_ARGS','SECRET_RESULT');").unwrap();
    temp
}

#[test]
fn desktop_ingestion_is_namespaced_bounded_and_private() {
    let temp = fixture();
    let snapshot = read_snapshot(Some(&temp.path().join("db.sqlite")));
    assert_eq!(snapshot.threads.len(), 2);
    assert_eq!(snapshot.threads[0].id, "ollama:chat-1");
    assert_eq!(snapshot.threads[0].model.as_deref(), Some("llama3.2"));
    assert_eq!(
        snapshot.threads[0].nickname.as_deref(),
        Some("Ollama Desktop llama3.2")
    );
    let encoded = serde_json::to_string(&snapshot).unwrap();
    assert!(!encoded.contains("SECRET_"));
    assert!(snapshot
        .threads
        .iter()
        .any(|thread| thread.id == "ollama:empty"
            && thread.state == codex_agent_monitor::model::ThreadState::Unknown));
}

#[test]
fn missing_and_malformed_databases_fail_gracefully() {
    let temp = tempfile::tempdir().unwrap();
    let missing = read_snapshot(Some(&temp.path().join("missing.sqlite")));
    assert!(missing.threads.is_empty());
    assert!(!missing.warnings.is_empty());
    let malformed = temp.path().join("bad.sqlite");
    fs::write(&malformed, b"not sqlite").unwrap();
    assert!(read_snapshot(Some(&malformed)).threads.is_empty());
}

#[test]
fn monitor_exposes_ollama_provider_and_raw_thread_filter() {
    let temp = fixture();
    let mut monitor = Monitor::new_with_sources_and_usage_and_ollama(
        Some(temp.path().join("codex").display().to_string()),
        None,
        None,
        Provider::Ollama,
        None,
        true,
        Some(temp.path().join("db.sqlite").display().to_string()),
    )
    .unwrap();
    let mut opts = filters(Provider::Ollama);
    opts.thread = Some("chat-1".to_string());
    let output = monitor
        .probe_snapshot(&opts, Default::default(), true)
        .unwrap();
    assert_eq!(output.query.provider.as_deref(), Some("ollama"));
    assert_eq!(output.threads[0].thread_id, "ollama:chat-1");
    assert_eq!(
        output.threads[0]
            .model
            .requested
            .value
            .as_ref()
            .and_then(|m| m.model.as_deref()),
        Some("llama3.2")
    );
}

#[test]
fn unavailable_local_api_is_non_fatal() {
    let status = poll_server_at("127.0.0.1:1");
    assert!(!status.available);
    let parsed = parse_ps_response("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"models\":[{\"name\":\"llama3.2\"}]}");
    assert!(parsed.available);
    assert_eq!(parsed.loaded_models, vec!["llama3.2"]);
}

#[test]
fn cli_parser_keeps_multiple_live_models_and_discards_prompt_args() {
    let now = Utc.timestamp_opt(1_800_000_000, 0).single().unwrap();
    let rows = parse_cli_processes(
        "100 00:05 ollama ollama run qwen3.8:27b SECRET_PROMPT\n101 01:02:03 ollama /usr/local/bin/ollama run llama3.2 SECRET_RESPONSE\n102 2-03:04:05 ollama ollama serve SECRET\n103 00:05 other ollama run ignored\nmalformed",
        now,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "ollama:cli:100");
    assert_eq!(rows[0].model.as_deref(), Some("qwen3.8:27b"));
    assert_eq!(rows[1].id, "ollama:cli:101");
    assert_eq!(rows[1].model.as_deref(), Some("llama3.2"));
    assert_eq!(rows[0].nickname.as_deref(), Some("ollama run qwen3.8:27b"));
    assert_ne!(
        rows[0].nickname.as_deref(),
        Some("Ollama Desktop qwen3.8:27b")
    );
    assert!(rows
        .iter()
        .all(|row| row.state == codex_agent_monitor::model::ThreadState::Running));
    let encoded = serde_json::to_string(&rows).unwrap();
    assert!(!encoded.contains("SECRET_"));
}

#[allow(dead_code)]
fn _path(_: &Path) {}
