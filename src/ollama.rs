//! Read-only ingestion for Ollama Desktop chats and local server metadata.
//!
//! The reader targets Ollama Desktop's known `chats`/`messages` metadata shape
//! and never selects columns that may contain prompts, responses, or tools.

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MAX_CHATS: usize = 500;
const RECENT_ACTIVITY_MINUTES: i64 = 15;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaThreadRecord {
    pub id: String,
    pub model: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub recency_at: Option<DateTime<Utc>>,
    pub state: crate::model::ThreadState,
    pub activity_at: Option<DateTime<Utc>>,
    pub nickname: Option<String>,
    pub source_kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OllamaSnapshot {
    pub threads: Vec<OllamaThreadRecord>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OllamaServerStatus {
    pub available: bool,
    pub loaded_models: Vec<String>,
    pub detail: Option<String>,
}

/// One model currently resident in the local Ollama server, as reported by
/// `/api/ps`. Residency is keep-alive evidence: the model was used recently and
/// is still loaded. It is not proof that a request is in flight right now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OllamaLoadedModel {
    pub name: String,
    pub expires_at: Option<DateTime<Utc>>,
}

pub fn default_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join("Library/Application Support/Ollama/db.sqlite"))
}

pub fn read_snapshot(path: Option<&Path>) -> OllamaSnapshot {
    let Some(path) = path.map(PathBuf::from).or_else(default_db_path) else {
        return OllamaSnapshot {
            warnings: vec!["Ollama Desktop database path unavailable".to_string()],
            ..Default::default()
        };
    };
    if !path.exists() {
        return OllamaSnapshot {
            warnings: vec![format!(
                "Ollama Desktop database missing: {}",
                path.display()
            )],
            ..Default::default()
        };
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = match Connection::open_with_flags(&path, flags) {
        Ok(conn) => conn,
        Err(err) => {
            return OllamaSnapshot {
                warnings: vec![format!(
                    "Unable to open Ollama Desktop database {}: {err}",
                    path.display()
                )],
                ..Default::default()
            }
        }
    };
    if let Err(err) = conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 250;") {
        return OllamaSnapshot {
            warnings: vec![format!(
                "Unable to configure Ollama Desktop database: {err}"
            )],
            ..Default::default()
        };
    }
    read_chats(&conn)
}

fn read_chats(conn: &Connection) -> OllamaSnapshot {
    let mut out = OllamaSnapshot::default();
    if !has_table(conn, "chats") || !has_table(conn, "messages") {
        out.warnings
            .push("Ollama Desktop chats/messages tables unavailable".to_string());
        return out;
    }
    let sql = format!(
        "SELECT c.id, c.created_at, m.model_name, m.created_at, m.updated_at, m.thinking_time_start, m.thinking_time_end \
         FROM chats c LEFT JOIN messages m ON m.id = (SELECT latest.id FROM messages latest WHERE latest.chat_id = c.id ORDER BY COALESCE(latest.updated_at, latest.created_at) DESC, latest.id DESC LIMIT 1) \
         ORDER BY COALESCE(m.updated_at, m.created_at, c.created_at) DESC LIMIT {MAX_CHATS}"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(err) => {
            out.warnings
                .push(format!("Unable to read Ollama Desktop chats: {err}"));
            return out;
        }
    };
    let mut rows = match stmt.query([]) {
        Ok(rows) => rows,
        Err(err) => {
            out.warnings
                .push(format!("Unable to query Ollama Desktop chats: {err}"));
            return out;
        }
    };
    while let Ok(Some(row)) = rows.next() {
        let raw_id: Option<String> = row.get(0).ok();
        let Some(raw_id) = raw_id.filter(|id| !id.is_empty()) else {
            continue;
        };
        let created_at = row.get_ref(1).ok().and_then(timestamp_value);
        let model = row
            .get::<_, Option<String>>(2)
            .ok()
            .flatten()
            .filter(|value| !value.is_empty());
        let message_created = row.get_ref(3).ok().and_then(timestamp_value);
        let updated_at = row
            .get_ref(4)
            .ok()
            .and_then(timestamp_value)
            .or(message_created);
        let thinking_start = row.get_ref(5).ok().and_then(timestamp_value);
        let thinking_end = row.get_ref(6).ok().and_then(timestamp_value);
        let recency_at = updated_at.or(created_at);
        let state = if thinking_start.is_some()
            && thinking_end.is_none()
            && thinking_start.is_some_and(|at| {
                at >= Utc::now() - chrono::Duration::minutes(RECENT_ACTIVITY_MINUTES)
            }) {
            crate::model::ThreadState::Running
        } else if message_created.is_some() {
            crate::model::ThreadState::Idle
        } else {
            crate::model::ThreadState::Unknown
        };
        out.threads.push(OllamaThreadRecord {
            id: format!("ollama:{raw_id}"),
            model: model.clone(),
            created_at,
            updated_at,
            recency_at,
            state,
            activity_at: recency_at,
            nickname: model
                .as_ref()
                .map(|model| format!("Ollama Desktop {model}")),
            source_kind: "ollama_desktop".to_string(),
        });
    }
    out
}

#[cfg(unix)]
pub fn read_cli_processes() -> OllamaSnapshot {
    let now = Utc::now();
    let output = match Command::new("ps")
        .args(["-axo", "pid=,etime=,comm=,args="])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(_) => {
            return OllamaSnapshot {
                warnings: vec!["Ollama CLI process listing unavailable".to_string()],
                ..Default::default()
            }
        }
        Err(_) => {
            return OllamaSnapshot {
                warnings: vec!["Ollama CLI process listing unavailable".to_string()],
                ..Default::default()
            }
        }
    };
    OllamaSnapshot {
        threads: parse_cli_processes(&String::from_utf8_lossy(&output.stdout), now),
        warnings: Vec::new(),
    }
}

#[cfg(not(unix))]
pub fn read_cli_processes() -> OllamaSnapshot {
    OllamaSnapshot {
        warnings: vec!["Ollama CLI process monitoring unavailable on this platform".to_string()],
        ..Default::default()
    }
}

pub fn parse_cli_processes(output: &str, now: DateTime<Utc>) -> Vec<OllamaThreadRecord> {
    output
        .lines()
        .filter_map(|line| parse_cli_process_line(line, now))
        .collect()
}

fn parse_cli_process_line(line: &str, now: DateTime<Utc>) -> Option<OllamaThreadRecord> {
    let mut fields = line.split_whitespace();
    let pid = fields.next()?.parse::<u32>().ok()?;
    let elapsed = parse_elapsed(fields.next()?)?;
    let comm = fields.next()?;
    if !is_ollama_executable(comm) {
        return None;
    }
    let executable = fields.next()?;
    if !is_ollama_executable(executable) || fields.next()? != "run" {
        return None;
    }
    let model = fields.next()?.to_string();
    if model.is_empty() {
        return None;
    }
    let recency_at = Some(now);
    let created_at = now.checked_sub_signed(chrono::Duration::seconds(elapsed));
    Some(OllamaThreadRecord {
        id: format!("ollama:cli:{pid}"),
        model: Some(model.clone()),
        created_at,
        updated_at: recency_at,
        recency_at,
        state: crate::model::ThreadState::Running,
        activity_at: recency_at,
        nickname: Some(format!("ollama run {model}")),
        source_kind: "ollama_cli".to_string(),
    })
}

fn is_ollama_executable(token: &str) -> bool {
    std::path::Path::new(token)
        .file_name()
        .is_some_and(|name| name == "ollama")
}

fn parse_elapsed(value: &str) -> Option<i64> {
    let (days, clock) = if let Some((days, clock)) = value.split_once('-') {
        (days.parse::<i64>().ok()?, clock)
    } else {
        (0, value)
    };
    let pieces = clock.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match pieces.as_slice() {
        [minutes, seconds] => (
            0,
            minutes.parse::<i64>().ok()?,
            seconds.parse::<i64>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<i64>().ok()?,
            minutes.parse::<i64>().ok()?,
            seconds.parse::<i64>().ok()?,
        ),
        _ => return None,
    };
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

fn has_table(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

fn timestamp_value(value: ValueRef<'_>) -> Option<DateTime<Utc>> {
    match value {
        ValueRef::Integer(raw) => {
            if raw > 100_000_000_000 {
                Utc.timestamp_millis_opt(raw).single()
            } else {
                Utc.timestamp_opt(raw, 0).single()
            }
        }
        ValueRef::Real(raw) => timestamp_value(ValueRef::Integer(raw as i64)),
        ValueRef::Text(text) => std::str::from_utf8(text)
            .ok()
            .and_then(parse_timestamp_text),
        _ => None,
    }
}

fn parse_timestamp_text(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .or_else(|| DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f%:z").ok())
        .map(|value| value.with_timezone(&Utc))
}

pub fn poll_server() -> OllamaServerStatus {
    poll_server_at("127.0.0.1:11434")
}

pub fn poll_server_at(address: &str) -> OllamaServerStatus {
    let mut stream = match std::net::TcpStream::connect_timeout(
        &match address.parse() {
            Ok(address) => address,
            Err(err) => {
                return OllamaServerStatus {
                    detail: Some(format!("Ollama API address unavailable: {err}")),
                    ..Default::default()
                }
            }
        },
        Duration::from_millis(250),
    ) {
        Ok(stream) => stream,
        Err(err) => {
            return OllamaServerStatus {
                detail: Some(format!("Ollama API unavailable: {err}")),
                ..Default::default()
            }
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(400)));
    use std::io::{Read, Write};
    if stream
        .write_all(b"GET /api/ps HTTP/1.1\r\nHost: 127.0.0.1:11434\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return OllamaServerStatus {
            detail: Some("Ollama API unavailable".to_string()),
            ..Default::default()
        };
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return OllamaServerStatus {
            detail: Some("Ollama API response unavailable".to_string()),
            ..Default::default()
        };
    }
    parse_ps_response(&response)
}

pub fn parse_ps_response(response: &str) -> OllamaServerStatus {
    let Some(body) = response.split("\r\n\r\n").nth(1) else {
        return OllamaServerStatus {
            detail: Some("Malformed Ollama API response".to_string()),
            ..Default::default()
        };
    };
    let parsed: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(_) => {
            return OllamaServerStatus {
                detail: Some("Malformed Ollama API response".to_string()),
                ..Default::default()
            }
        }
    };
    let loaded_models = parse_loaded_models(&parsed)
        .into_iter()
        .map(|model| model.name)
        .collect();
    OllamaServerStatus {
        available: response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"),
        loaded_models,
        detail: None,
    }
}

/// Extract the resident models of an `/api/ps` body. Only the model name and
/// its keep-alive expiry are read; prompts and responses are never present in
/// this endpoint and no other field is retained.
pub fn parse_loaded_models(parsed: &Value) -> Vec<OllamaLoadedModel> {
    parsed
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let name = model
                .get("name")
                .or_else(|| model.get("model"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string)?;
            let expires_at = model
                .get("expires_at")
                .and_then(Value::as_str)
                .and_then(parse_timestamp_text);
            Some(OllamaLoadedModel { name, expires_at })
        })
        .take(MAX_CHATS)
        .collect()
}

/// Build one thread row per model resident in the local Ollama server so API
/// consumers (Codex, Claude, Kiro, or any other client) surface as running work
/// instead of only as a server status line.
pub fn api_threads(status: &OllamaServerStatus, now: DateTime<Utc>) -> Vec<OllamaThreadRecord> {
    if !status.available {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    status
        .loaded_models
        .iter()
        .filter(|model| !model.is_empty())
        .filter(|model| seen.insert((*model).clone()))
        .map(|model| OllamaThreadRecord {
            id: format!("ollama:api:{model}"),
            model: Some(model.clone()),
            created_at: None,
            updated_at: Some(now),
            recency_at: Some(now),
            state: crate::model::ThreadState::Running,
            activity_at: Some(now),
            nickname: Some(format!("Ollama serving {model}")),
            source_kind: "ollama_api".to_string(),
        })
        .collect()
}
