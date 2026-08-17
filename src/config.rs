use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use toml::Value as TomlValue;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub model: Option<String>,
    pub model_reasoning_effort: Option<String>,
    pub sqlite_home: Option<PathBuf>,
}

pub fn resolve_codex_home(cli_home: Option<&str>) -> Result<PathBuf> {
    if let Some(home) = cli_home {
        return Ok(PathBuf::from(home));
    }

    if let Some(env_home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(env_home));
    }

    dirs::home_dir()
        .map(|h| h.join(".codex"))
        .context("Unable to resolve home dir for default CODEX_HOME")
}

#[derive(Debug, Clone)]
pub struct ConfigContext {
    pub global: Option<GlobalConfig>,
    pub agents_dir: Option<PathBuf>,
    pub home: PathBuf,
}

#[derive(Debug, Clone)]
pub enum SqliteHome {
    Directory(PathBuf),
    File(PathBuf),
}

impl SqliteHome {
    pub fn db_path(&self) -> PathBuf {
        match self {
            SqliteHome::Directory(dir) => dir.join("state_5.sqlite"),
            SqliteHome::File(file) => file.clone(),
        }
    }
}

pub fn load_config(home: &Path) -> Result<ConfigContext> {
    let mut global = None;
    let config_path = home.join("config.toml");
    let mut agents_dir = None;
    if config_path.exists() {
        let raw = fs::read_to_string(&config_path)
            .with_context(|| format!("read config at {}", config_path.display()))?;
        let parsed: TomlValue = toml::from_str(&raw).context("parse config.toml")?;
        let model =
            pick_string(&parsed, &["model"]).or_else(|| pick_string(&parsed, &["agent", "model"]));
        let effort = pick_string(&parsed, &["model_reasoning_effort"])
            .or_else(|| pick_string(&parsed, &["model", "reasoning_effort"]));
        let sqlite_home = pick_string(&parsed, &["sqlite_home"]).map(PathBuf::from);
        global = Some(GlobalConfig {
            model,
            model_reasoning_effort: effort,
            sqlite_home,
        });

        if let Some(dir) = pick_string(&parsed, &["agents", "dir"]) {
            agents_dir = Some(home.join(dir));
        } else if let Some(dir) = pick_string(&parsed, &["agents_dir"]) {
            agents_dir = Some(home.join(dir));
        }
    }
    Ok(ConfigContext {
        global,
        agents_dir,
        home: home.to_path_buf(),
    })
}

pub fn sqlite_home(config_home: &Path, cfg: &ConfigContext) -> SqliteHome {
    if let Some(c) = &cfg.global {
        if let Some(path) = &c.sqlite_home {
            let p = if path.is_absolute() {
                path.clone()
            } else {
                config_home.join(path)
            };
            if is_sqlite_file_path(&p) {
                return SqliteHome::File(p);
            }
            return SqliteHome::Directory(p);
        }
    }
    if let Ok(env) = std::env::var("CODEX_SQLITE_HOME") {
        let env_path = PathBuf::from(env);
        let p = if env_path.is_absolute() {
            env_path
        } else {
            config_home.join(env_path)
        };
        if is_sqlite_file_path(&p) {
            return SqliteHome::File(p);
        }
        return SqliteHome::Directory(p);
    }
    SqliteHome::Directory(config_home.to_path_buf())
}

fn is_sqlite_file_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("sqlite"))
}

pub fn load_agent_config_value(
    agent_path: &Path,
    key: &str,
    fallback: Option<&str>,
) -> Option<String> {
    let agent_path = if agent_path.is_relative() {
        return fallback.map(ToString::to_string);
    } else {
        agent_path.to_path_buf()
    };
    let bytes = fs::read_to_string(&agent_path).ok()?;
    if let Ok(table) = serde_json::from_str::<HashMap<String, Value>>(&bytes) {
        if let Some(value) = table.get(key) {
            if let Some(s) = value.as_str() {
                return Some(s.to_string());
            }
            if let Some(value) = value.get("value") {
                if let Some(s) = value.as_str() {
                    return Some(s.to_string());
                }
            }
        }
    }
    if let Ok(toml_value) = bytes.parse::<TomlValue>() {
        if let Some(value) = toml_value.get(key).and_then(TomlValue::as_str) {
            return Some(value.to_string());
        }
        if let TomlValue::Table(table) = toml_value {
            if let Some(value) = table.get(key).and_then(TomlValue::as_str) {
                return Some(value.to_string());
            }
        }
    }
    fallback.map(ToString::to_string)
}

fn pick_string(value: &TomlValue, path: &[&str]) -> Option<String> {
    let mut cur = value;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str().map(ToString::to_string)
}
