use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{types::ValueRef, Connection, OpenFlags, Row};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DbThreadRecord {
    pub id: String,
    pub rollout_path: Option<PathBuf>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub recency_at: Option<DateTime<Utc>>,
    pub source: Option<Value>,
    pub thread_source: Option<Value>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub agent_path: Option<String>,
    pub cwd: Option<String>,
    pub source_kind: Option<String>,
    pub raw: HashMap<String, Value>,
}

#[derive(Debug)]
pub struct DbSnapshot {
    pub threads: Vec<DbThreadRecord>,
    pub parent_edges: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

pub fn read_state_db(db_path: &Path) -> Result<DbSnapshot> {
    if !db_path.exists() {
        return Ok(DbSnapshot {
            threads: Vec::new(),
            parent_edges: Vec::new(),
            warnings: vec![format!("state db missing: {}", db_path.display())],
        });
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = match Connection::open_with_flags(db_path, flags) {
        Ok(conn) => conn,
        Err(err) => {
            return Ok(DbSnapshot {
                threads: Vec::new(),
                parent_edges: Vec::new(),
                warnings: vec![format!(
                    "Unable to open sqlite db {}: {err}",
                    db_path.display()
                )],
            });
        }
    };

    conn.execute_batch("PRAGMA query_only = ON;\nPRAGMA busy_timeout = 250;")
        .context("set sqlite pragmas")?;

    if !table_exists(&conn, "threads")? {
        return Ok(DbSnapshot {
            threads: Vec::new(),
            parent_edges: Vec::new(),
            warnings: vec!["threads table missing; persisted state unavailable".to_string()],
        });
    }

    let threads = read_threads(&conn)?;
    let parent_edges = read_thread_spawn_edges(&conn)?;
    Ok(DbSnapshot {
        threads,
        parent_edges,
        warnings: Vec::new(),
    })
}

fn table_exists(conn: &Connection, table_name: &str) -> Result<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1")?;
    let exists = stmt.exists([table_name])?;
    Ok(exists)
}

fn read_threads(conn: &Connection) -> Result<Vec<DbThreadRecord>> {
    let mut threads = Vec::new();
    let mut stmt = conn.prepare("SELECT * FROM threads")?;
    let col_names = stmt
        .column_names()
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let mut map = HashMap::new();
        for (i, key) in col_names.iter().enumerate() {
            let key_lower = key.to_lowercase();
            if let Some(value) = sqlite_value_to_json(row, i)? {
                map.insert(key_lower, value);
            }
        }

        let id = get_string(&map, "id").unwrap_or_default();
        if id.is_empty() {
            continue;
        }

        let rollout_path = get_string(&map, "rollout_path").map(PathBuf::from);
        let source = get_json(&map, "source");
        let thread_source = get_json(&map, "thread_source");
        let model = get_string(&map, "model");
        let reasoning_effort = get_string(&map, "reasoning_effort")
            .or_else(|| get_string(&map, "model_reasoning_effort"))
            .or_else(|| get_string(&map, "model_reasoning"));

        let created_at =
            get_timestamp(&map, "created_at_ms").or_else(|| get_timestamp(&map, "created_at"));
        let updated_at =
            get_timestamp(&map, "updated_at_ms").or_else(|| get_timestamp(&map, "updated_at"));
        let recency_at = get_timestamp(&map, "recency_at_ms")
            .or_else(|| get_timestamp(&map, "recency_at"))
            .or_else(|| updated_at.or(created_at));
        let agent_nickname = get_string(&map, "agent_nickname");
        let agent_role = get_string(&map, "agent_role").or_else(|| get_string(&map, "role"));
        let agent_path = get_string(&map, "agent_path");
        let cwd = get_string(&map, "cwd").or_else(|| get_string(&map, "working_directory"));
        let source_kind = derive_source_kind(&source, thread_source.as_ref());

        threads.push(DbThreadRecord {
            id,
            rollout_path,
            created_at,
            updated_at,
            recency_at,
            source,
            thread_source,
            model,
            reasoning_effort,
            agent_nickname,
            agent_role,
            agent_path,
            cwd,
            source_kind,
            raw: map,
        });
    }
    Ok(threads)
}

fn derive_source_kind(source: &Option<Value>, thread_source: Option<&Value>) -> Option<String> {
    if let Some(source) = source {
        match source {
            Value::String(s) => Some(s.to_lowercase()),
            Value::Object(map) => {
                if map.iter().any(|(k, _)| k.eq_ignore_ascii_case("subagent")) {
                    Some("subagent".to_string())
                } else {
                    None
                }
            }
            Value::Array(_) => None,
            _ => None,
        }
    } else if let Some(thread_source) = thread_source {
        if let Some(source_kind) = thread_source.as_str() {
            return Some(source_kind.to_lowercase());
        }
        if let Some(source_kind) = thread_source.get("kind").and_then(Value::as_str) {
            return Some(source_kind.to_lowercase());
        }
        None
    } else {
        None
    }
}

fn read_thread_spawn_edges(conn: &Connection) -> Result<Vec<(String, String)>> {
    if !table_exists(conn, "thread_spawn_edges")? {
        return Ok(Vec::new());
    }
    let mut stmt =
        match conn.prepare("SELECT parent_thread_id, child_thread_id FROM thread_spawn_edges") {
            Ok(stmt) => stmt,
            Err(_) => return Ok(Vec::new()),
        };
    let mut rows = stmt.query([])?;
    let mut edges = Vec::new();
    while let Some(row) = rows.next()? {
        let parent = get_string_from_row(row, 0)?;
        let child = get_string_from_row(row, 1)?;
        if let (Some(parent), Some(child)) = (parent, child) {
            if !parent.is_empty() && !child.is_empty() && parent != child {
                edges.push((parent, child));
            }
        }
    }
    Ok(edges)
}

fn sqlite_value_to_json(row: &Row<'_>, index: usize) -> Result<Option<Value>> {
    let val = row.get_ref(index)?;
    let out = match val {
        rusqlite::types::ValueRef::Null => None,
        rusqlite::types::ValueRef::Integer(x) => Some(Value::from(x)),
        rusqlite::types::ValueRef::Real(x) => Some(Value::from(x)),
        rusqlite::types::ValueRef::Text(x) => {
            let s = String::from_utf8_lossy(x);
            if let Ok(json) = serde_json::from_str(&s) {
                Some(json)
            } else {
                Some(Value::String(s.to_string()))
            }
        }
        rusqlite::types::ValueRef::Blob(x) => {
            Some(Value::String(String::from_utf8_lossy(x).to_string()))
        }
    };
    Ok(out)
}

fn get_string(map: &HashMap<String, Value>, key: &str) -> Option<String> {
    map.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str().map(ToString::to_string))
}

fn get_json(map: &HashMap<String, Value>, key: &str) -> Option<Value> {
    let value = map
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())?;
    parse_json_like_value(value)
}

fn get_timestamp(map: &HashMap<String, Value>, key: &str) -> Option<DateTime<Utc>> {
    map.iter().find_map(|(k, v)| {
        if !k.eq_ignore_ascii_case(key) {
            return None;
        }
        parse_datetime(v)
    })
}

fn get_string_from_row(row: &Row<'_>, index: usize) -> Result<Option<String>> {
    let raw = row.get_ref(index)?;
    let value = match raw {
        ValueRef::Text(text) => Some(String::from_utf8_lossy(text).to_string()),
        ValueRef::Integer(x) => Some(x.to_string()),
        ValueRef::Real(v) => Some(v.to_string()),
        ValueRef::Blob(v) => Some(String::from_utf8_lossy(v).to_string()),
        ValueRef::Null => None,
    };
    Ok(value)
}

fn parse_datetime(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(num) => num
            .as_i64()
            .or_else(|| num.as_f64().map(|v| v.round() as i64))
            .and_then(parse_numeric_timestamp),
        Value::String(raw) => {
            if let Ok(sec) = raw.parse::<i64>() {
                return parse_numeric_timestamp(sec);
            }
            chrono::DateTime::parse_from_rfc3339(raw)
                .ok()
                .map(|x| x.with_timezone(&Utc))
        }
        _ => None,
    }
}

fn parse_numeric_timestamp(raw: i64) -> Option<DateTime<Utc>> {
    if raw > 10_000_000_000 {
        DateTime::from_timestamp_millis(raw)
    } else {
        DateTime::from_timestamp(raw, 0)
    }
}

fn parse_json_like_value(value: Value) -> Option<Value> {
    match value {
        Value::String(s) => serde_json::from_str(&s).ok(),
        _ => Some(value),
    }
}

pub fn db_path(home: &Path) -> PathBuf {
    home.join("state_5.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_datetime_prefers_milliseconds_for_large_integers() {
        let value = Value::Number(serde_json::Number::from(1_720_000_000_000_i64));
        let parsed = parse_datetime(&value).expect("parsed");
        assert_eq!(parsed.timestamp_millis(), 1_720_000_000_000);
    }

    #[test]
    fn parse_datetime_prefers_seconds_for_small_integers() {
        let value = Value::Number(serde_json::Number::from(1_720_000_001_i64));
        let parsed = parse_datetime(&value).expect("parsed");
        assert_eq!(parsed.timestamp(), 1_720_000_001);
    }
}
