use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::process::Command;

use crate::cli::{FilterOpts, ThreadStateFilter};
use crate::config::{
    load_agent_config_value, load_config, resolve_codex_home, sqlite_home, SqliteHome,
};
use crate::db::DbThreadRecord;
use crate::model::{
    Confidence, EvidenceSource, ModelSpec, ModelSummary, Observed, ProbeEnvironment, ProbeOutput,
    QueryInfo, ThreadActivity, ThreadEvidence, ThreadSnapshot, ThreadState,
};
use crate::rollout::{RolloutParseResult, RolloutStateHint};
use crate::runtime::RuntimeOverlay;

#[derive(Debug)]
pub struct Monitor {
    pub codex_home: PathBuf,
    pub config: crate::config::ConfigContext,
    rollout_cache: HashMap<PathBuf, CachedRollout>,
}

#[derive(Debug, Clone)]
struct CachedRollout {
    path_mtime: Option<std::time::SystemTime>,
    path_size: u64,
    parsed: RolloutParseResult,
}

impl Monitor {
    pub fn new(override_home: Option<String>) -> Result<Self> {
        let home = resolve_codex_home(override_home.as_deref())?;
        let config = load_config(&home)?;
        Ok(Monitor {
            codex_home: home,
            config,
            rollout_cache: HashMap::new(),
        })
    }

    pub fn probe_snapshot(
        &mut self,
        filters: &FilterOpts,
        runtime: RuntimeOverlay,
        allow_partial_rollout: bool,
    ) -> Result<ProbeOutput> {
        let sqlite_root = sqlite_home(&self.codex_home, &self.config);
        let db_path = match sqlite_root {
            SqliteHome::Directory(dir) => dir.join("state_5.sqlite"),
            SqliteHome::File(file) => file,
        };

        let db = crate::db::read_state_db(&db_path)?;
        let mut warnings = db.warnings;
        let mut rollouts: HashMap<String, RolloutParseResult> = HashMap::new();

        let candidate_threads: Vec<&crate::db::DbThreadRecord> = db
            .threads
            .iter()
            .filter(|thread| {
                if let Some(project_filter) = &filters.project {
                    if let Some(cwd) = thread.cwd.clone() {
                        if !paths_match(&cwd, project_filter) {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
                if let Some(role_filter) = &filters.role {
                    if !thread
                        .agent_role
                        .as_deref()
                        .is_some_and(|role| role.eq_ignore_ascii_case(role_filter))
                    {
                        return false;
                    }
                }
                true
            })
            .collect();

        let cutoff = filters
            .recent_minutes
            .filter(|_| !filters.all)
            .and_then(|m| Utc::now().checked_sub_signed(ChronoDuration::minutes(m as i64)));

        let selected_ids: HashSet<String> = candidate_threads
            .iter()
            .filter(|thread| matches_recent_filter(thread, &cutoff))
            .filter(|thread| {
                filters
                    .thread
                    .as_ref()
                    .is_none_or(|needle| thread.id == *needle)
            })
            .map(|thread| thread.id.clone())
            .collect();

        for thread_id in &selected_ids {
            if let Some(thread) = db
                .threads
                .iter()
                .find(|t| &t.id == thread_id)
                .and_then(|t| t.rollout_path.as_ref())
            {
                if let Some(parsed) = read_rollout_cached(
                    &mut self.rollout_cache,
                    thread,
                    thread_id,
                    allow_partial_rollout,
                ) {
                    warnings.extend(parsed.warnings.clone());
                    rollouts.insert(thread_id.clone(), parsed);
                }
            }
        }

        let hints = crate::tree::collect_parent_hints(&db.threads, &rollouts);
        let (edges, parent_sources) =
            crate::tree::build_parent_edges_with_sources(db.parent_edges.clone(), &hints);
        let mut parent_of = HashMap::new();
        for (parent, child) in &edges {
            parent_of.insert(child.clone(), parent.clone());
        }

        let mut thread_rows = Vec::new();
        for thread in candidate_threads {
            if !selected_ids.contains(&thread.id) {
                continue;
            }
            let hint_state = rollouts.get(&thread.id).map(|r| r.final_state);
            let state = map_state(hint_state);
            if let Some(state_filter) = &filters.state {
                if !applies_state(state_filter, &state) {
                    continue;
                }
            }
            if !matches_recent_filter(thread, &cutoff) {
                continue;
            }
            if let Some(thread_filter) = &filters.thread {
                if &thread.id != thread_filter {
                    continue;
                }
            }

            let requested = requested_model(thread, rollouts.get(&thread.id));
            let configured = configured_model(thread, &self.config);
            let mut effective = Observed::unknown();
            if let Some(value) = runtime.effective_model.get(&thread.id) {
                effective = value.clone();
            }
            let mut activity = Vec::new();
            if let Some(parsed) = rollouts.get(&thread.id) {
                let mut reversed = parsed.activity.iter().rev().take(25).collect::<Vec<_>>();
                reversed.reverse();
                for act in reversed {
                    activity.push(ThreadActivity {
                        kind: act.kind.clone(),
                        tool_name: act.tool_name.clone(),
                        status: act.status.clone(),
                        timestamp: act.ts,
                    });
                }
            }

            let (thread_state, state_detail) = match state {
                ThreadState::Done => (ThreadState::Idle, Some("turn_completed".to_string())),
                ThreadState::Idle => (ThreadState::Idle, Some("idle".to_string())),
                ThreadState::Running => (ThreadState::Running, Some("running".to_string())),
                ThreadState::Failed => (ThreadState::Failed, Some("failed".to_string())),
                ThreadState::Interrupted => {
                    (ThreadState::Interrupted, Some("interrupted".to_string()))
                }
                ThreadState::Unknown => (ThreadState::Unknown, Some("unknown".to_string())),
            };

            let mut children = Vec::new();
            for (child, parent) in &parent_of {
                if parent == &thread.id {
                    children.push(child.clone());
                }
            }
            children.sort_unstable();
            let parent_thread_id = parent_of.get(&thread.id).cloned();
            let parent_source = parent_sources.get(&thread.id).copied();
            let evidence = thread_evidence(
                thread,
                parent_thread_id.clone(),
                parent_source,
                rollouts.get(&thread.id),
                thread_state.clone(),
                &activity,
            );
            let nickname = evidence.nickname.value.clone();
            let role = evidence.role.value.clone();
            let parent_thread_id = evidence.parent_thread_id.value.clone();
            let cwd = evidence.cwd.value.clone();
            let source_kind = evidence.source_kind.value.clone();

            thread_rows.push(ThreadSnapshot {
                thread_id: thread.id.clone(),
                nickname,
                role,
                parent_thread_id: parent_thread_id.clone(),
                cwd,
                source_kind,
                children,
                project: thread_project(thread),
                state: thread_state.clone(),
                state_detail,
                model: ModelSummary {
                    configured,
                    requested,
                    effective,
                    rerouted_from: runtime.rerouted_from.get(&thread.id).cloned(),
                    reroute_reason: runtime.reroute_reason.get(&thread.id).cloned(),
                },
                created_at: thread.created_at,
                updated_at: thread.updated_at,
                recency_at: thread.recency_at,
                rollout_path: thread
                    .rollout_path
                    .as_ref()
                    .map(|p| p.display().to_string()),
                warnings: Vec::new(),
                recent_activity: activity,
                evidence,
            });
        }

        thread_rows.sort_by(|a, b| {
            b.recency_at
                .unwrap_or_else(Utc::now)
                .cmp(&a.recency_at.unwrap_or_else(Utc::now))
        });
        if let Some(depth) = filters.depth {
            thread_rows = apply_depth_filter(thread_rows, &parent_of, depth);
        }

        let ids = thread_rows
            .iter()
            .map(|x| x.thread_id.clone())
            .collect::<Vec<_>>();
        let mut out_edges = Vec::new();
        for child in &ids {
            if let Some(parent) = parent_of.get(child) {
                if ids.iter().any(|x| x == parent) {
                    out_edges.push((parent.clone(), child.clone()));
                }
            }
        }
        let tree = crate::tree::build_thread_tree(&ids, &out_edges);

        let mut combined_warnings = Vec::new();
        combined_warnings.extend(warnings);
        combined_warnings.extend(runtime.warnings);
        let environment = detect_environment();

        Ok(ProbeOutput {
            schema_version: "codex-agent-monitor.probe.v1".to_string(),
            generated_at: Utc::now(),
            codex_home: self.codex_home.display().to_string(),
            environment,
            query: QueryInfo {
                include_all: filters.all,
                project: filters.project.clone(),
                thread: filters.thread.clone(),
                state: filters
                    .state
                    .as_ref()
                    .map(|s| format!("{:?}", s).to_lowercase()),
                role: filters.role.clone(),
                depth: filters.depth,
            },
            warnings: combined_warnings,
            threads: thread_rows,
            tree,
        })
    }
}

fn read_rollout_cached(
    cache: &mut HashMap<PathBuf, CachedRollout>,
    path: &Path,
    thread_id: &str,
    allow_partial: bool,
) -> Option<RolloutParseResult> {
    let metadata = match fs::metadata(path) {
        Ok(md) => md,
        Err(_) => return None,
    };
    let size = metadata.len();
    let mtime = metadata.modified().ok();

    if let Some(existing) = cache.get(path) {
        if existing.path_size == size && existing.path_mtime == mtime {
            return Some(existing.parsed.clone());
        }
    }

    let parsed = crate::rollout::parse_rollout_file(path, thread_id, allow_partial);
    cache.insert(
        path.to_path_buf(),
        CachedRollout {
            path_mtime: mtime,
            path_size: size,
            parsed: parsed.clone(),
        },
    );
    Some(parsed)
}

fn paths_match(candidate: &str, filter: &str) -> bool {
    canonicalize_path(candidate) == canonicalize_path(filter)
}

fn canonicalize_path(value: &str) -> Option<String> {
    let path = Path::new(value);
    if path.is_absolute() {
        Some(normalize_path(path))
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| normalize_path(&cwd.join(path)))
    }
}

#[cfg(windows)]
fn strip_verbatim_prefix(value: &str) -> String {
    let unc = r"\\?\UNC\";
    let verbatim = r"\\?\";
    if let Some(rest) = value.strip_prefix(unc) {
        format!(r"\\{rest}")
    } else if let Some(rest) = value.strip_prefix(verbatim) {
        rest.to_string()
    } else {
        value.to_string()
    }
}

#[cfg(windows)]
fn normalize_path(path: &Path) -> String {
    let stripped = strip_verbatim_prefix(&path.to_string_lossy());
    let cleaned_path = stripped.replace('/', "\\");
    let cleaned = Path::new(&cleaned_path);
    let mut components = Vec::new();
    for component in cleaned.components() {
        match component {
            Component::Prefix(prefix) => {
                components.push(prefix.as_os_str().to_string_lossy().to_string())
            }
            Component::RootDir => {
                if !components.is_empty()
                    && !components.last().is_some_and(|part| part.ends_with('\\'))
                {
                    if let Some(last) = components.last_mut() {
                        last.push('\\');
                    }
                }
            }
            Component::Normal(segment) => components.push(segment.to_string_lossy().to_string()),
            Component::CurDir | Component::ParentDir => {}
        }
    }

    let normalized = components.join("\\");
    normalized.to_lowercase()
}

#[cfg(not(windows))]
fn normalize_path(path: &Path) -> String {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::ParentDir => None,
            Component::CurDir => None,
            Component::Normal(seg) => Some(seg.to_string_lossy().to_string()),
            Component::RootDir => Some(String::new()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if components.first().is_some_and(String::is_empty) {
        format!("/{}", components[1..].join("/"))
    } else {
        components.join("/")
    }
}

fn detect_environment() -> ProbeEnvironment {
    let os = std::env::consts::OS.to_string();
    let mut codex_cli_path = None;
    let mut codex_cli_version = None;
    for path in find_command_in_path("codex") {
        if let Ok(output) = Command::new(&path).arg("--version").output() {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                codex_cli_version = Some(text.lines().next().unwrap_or("").trim().to_string());
                codex_cli_path = Some(path);
                break;
            }
        }
    }
    ProbeEnvironment {
        os,
        codex_cli_path,
        codex_cli_version,
    }
}

fn find_command_in_path(command: &str) -> Vec<String> {
    find_command_in_path_with_env(command, None, None)
}

fn find_command_in_path_with_env(
    command: &str,
    path_var: Option<&str>,
    path_ext_var: Option<&str>,
) -> Vec<String> {
    let command_path = std::path::Path::new(command);
    if command_path.is_absolute() && command_path.exists() {
        return vec![command.to_string()];
    }

    let path_var = path_var
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"));
    let Some(path_var) = path_var else {
        return Vec::new();
    };
    let paths = std::env::split_paths(&path_var).collect::<Vec<_>>();
    if paths.is_empty() {
        return Vec::new();
    }

    let has_extension = command_path.extension().is_some();
    let mut results = Vec::new();

    for dir in paths {
        if cfg!(windows) && !has_extension {
            let mut pathext = String::new();
            if let Some(value) = path_ext_var {
                pathext = value.to_string();
            } else if let Some(value) = std::env::var_os("PATHEXT") {
                pathext = value.to_string_lossy().to_string();
            } else {
                pathext.push_str(".exe;.cmd;.bat;.com");
            }
            let pathext = pathext.to_ascii_lowercase();
            let known_exts = [".exe", ".cmd", ".bat", ".com"];
            let mut ordered_exts: Vec<String> = Vec::new();
            for ext in pathext.split(';').filter_map(|item| {
                let trimmed = item.trim();
                if trimmed.is_empty() {
                    return None;
                }
                let normalized = if trimmed.starts_with('.') {
                    trimmed.to_string()
                } else {
                    format!(".{trimmed}")
                };
                if known_exts.contains(&normalized.as_str()) {
                    Some(normalized)
                } else {
                    None
                }
            }) {
                if !ordered_exts.iter().any(|existing| existing == &ext) {
                    ordered_exts.push(ext);
                }
            }
            if ordered_exts.is_empty() {
                ordered_exts = known_exts.iter().map(|ext| ext.to_string()).collect();
            }
            for ext in ordered_exts {
                let candidate = dir.join(format!("{command}{}", ext));
                if candidate.exists() {
                    results.push(candidate.to_string_lossy().to_string());
                }
            }
        }
        let direct = dir.join(command);
        if direct.exists() {
            results.push(direct.to_string_lossy().to_string());
        }
    }

    results
}

fn matches_recent_filter(thread: &DbThreadRecord, cutoff: &Option<DateTime<Utc>>) -> bool {
    if let Some(cutoff) = cutoff {
        return thread
            .recency_at
            .or(thread.updated_at)
            .or(thread.created_at)
            .is_some_and(|ts| ts >= *cutoff);
    }
    true
}

fn applies_state(filter: &ThreadStateFilter, state: &ThreadState) -> bool {
    match filter {
        ThreadStateFilter::Running => matches!(state, ThreadState::Running),
        ThreadStateFilter::Idle => matches!(state, ThreadState::Idle | ThreadState::Done),
        ThreadStateFilter::Interrupted => matches!(state, ThreadState::Interrupted),
        ThreadStateFilter::Failed => matches!(state, ThreadState::Failed),
        ThreadStateFilter::Unknown => matches!(state, ThreadState::Unknown),
    }
}

fn map_state(hint: Option<RolloutStateHint>) -> ThreadState {
    match hint.unwrap_or(RolloutStateHint::Unknown) {
        RolloutStateHint::Running => ThreadState::Running,
        RolloutStateHint::Idle => ThreadState::Idle,
        RolloutStateHint::TurnCompleted => ThreadState::Done,
        RolloutStateHint::Interrupted => ThreadState::Interrupted,
        RolloutStateHint::Failed => ThreadState::Failed,
        RolloutStateHint::Unknown => ThreadState::Unknown,
    }
}

fn thread_source_for_parent(source: Option<crate::tree::ParentSource>) -> &'static str {
    match source.unwrap_or(crate::tree::ParentSource::RolloutHint) {
        crate::tree::ParentSource::ThreadSpawnEdge => "thread_spawn_edges",
        crate::tree::ParentSource::SourceHint => "source-hint",
        crate::tree::ParentSource::RolloutHint => "persisted-session-meta",
    }
}

fn thread_evidence(
    thread: &crate::db::DbThreadRecord,
    parent_thread_id: Option<String>,
    parent_source: Option<crate::tree::ParentSource>,
    rollout: Option<&RolloutParseResult>,
    state: ThreadState,
    activity: &[ThreadActivity],
) -> ThreadEvidence {
    let state = state.clone();
    let state_is_unknown = matches!(state, ThreadState::Unknown);
    let nickname = thread.agent_nickname.clone().or_else(|| {
        rollout
            .and_then(|r| r.canonical_nickname.clone())
            .filter(|n| !n.is_empty())
    });
    let role = thread.agent_role.clone().or_else(|| {
        rollout
            .and_then(|r| r.canonical_role.clone())
            .filter(|n| !n.is_empty())
    });
    let cwd = thread.cwd.clone();
    let source_kind = thread.source_kind.clone();

    let latest_activity = activity.last().cloned().map(|activity| {
        let confidence = if activity.timestamp.is_some() {
            Confidence::Medium
        } else {
            Confidence::Low
        };
        Observed {
            value: Some(activity),
            source: Some(EvidenceSource {
                kind: "persisted-session".to_string(),
                detail: Some("latest activity".to_string()),
            }),
            observed_at: None,
            confidence,
            detail: Some("persisted session activity".to_string()),
        }
    });

    ThreadEvidence {
        nickname: nickname
            .map(|value| {
                Observed::from_value(
                    Some(value),
                    if thread.agent_nickname.is_some() {
                        "threads-table"
                    } else {
                        "persisted-session-meta"
                    },
                    Some(if thread.agent_nickname.is_some() {
                        "from threads table".to_string()
                    } else {
                        "from persisted-session meta".to_string()
                    }),
                    None,
                )
            })
            .unwrap_or_else(Observed::unknown),
        role: role
            .map(|value| {
                Observed::from_value(
                    Some(value),
                    if thread.agent_role.is_some() {
                        "threads-table"
                    } else {
                        "persisted-session-meta"
                    },
                    Some(if thread.agent_role.is_some() {
                        "from threads table".to_string()
                    } else {
                        "from persisted-session meta".to_string()
                    }),
                    None,
                )
            })
            .unwrap_or_else(Observed::unknown),
        parent_thread_id: parent_thread_id
            .map(|value| {
                Observed::from_value(
                    Some(value),
                    thread_source_for_parent(parent_source),
                    Some("from inferred thread parent".to_string()),
                    None,
                )
            })
            .unwrap_or_else(Observed::unknown),
        state: Observed {
            value: Some(state),
            source: Some(EvidenceSource {
                kind: "persisted-session".to_string(),
                detail: Some("derived from rollout lifecycle".to_string()),
            }),
            observed_at: None,
            confidence: if state_is_unknown {
                Confidence::Low
            } else {
                Confidence::Medium
            },
            detail: Some("thread state".to_string()),
        },
        cwd: cwd
            .map(|value| {
                Observed::from_value(
                    Some(value),
                    "threads-table",
                    Some("from threads table".to_string()),
                    None,
                )
            })
            .unwrap_or_else(Observed::unknown),
        source_kind: source_kind
            .map(|value| {
                Observed::from_value(
                    Some(value),
                    "threads-table",
                    Some("from threads table".to_string()),
                    None,
                )
            })
            .unwrap_or_else(Observed::unknown),
        latest_activity: latest_activity.unwrap_or_else(Observed::unknown),
    }
}

fn configured_model(
    thread: &crate::db::DbThreadRecord,
    cfg: &crate::config::ConfigContext,
) -> Observed<ModelSpec> {
    let mut model = cfg.global.as_ref().and_then(|g| g.model.clone());
    let mut effort = cfg
        .global
        .as_ref()
        .and_then(|g| g.model_reasoning_effort.clone());

    if let Some(agent_path) = &thread.agent_path {
        let resolved = if PathBuf::from(agent_path).is_absolute() {
            PathBuf::from(agent_path)
        } else if let Some(base) = &cfg.agents_dir {
            base.join(agent_path)
        } else {
            cfg.home.join(agent_path)
        };
        model = load_agent_config_value(&resolved, "model", model.as_deref()).or(model);
        effort = load_agent_config_value(&resolved, "model_reasoning_effort", effort.as_deref())
            .or(effort);
    }

    if model.is_none() && effort.is_none() {
        Observed {
            value: None,
            source: Some(EvidenceSource {
                kind: "unknown".to_string(),
                detail: Some("configured values unavailable".to_string()),
            }),
            observed_at: None,
            confidence: Confidence::Low,
            detail: Some("No configured model evidence".to_string()),
        }
    } else {
        Observed {
            value: Some(ModelSpec {
                model,
                reasoning_effort: effort,
            }),
            source: Some(EvidenceSource {
                kind: "config.toml|agent file".to_string(),
                detail: Some("best-effort read from configured sources".to_string()),
            }),
            observed_at: None,
            confidence: Confidence::Medium,
            detail: Some("configured values".to_string()),
        }
    }
}

fn requested_model(
    thread: &crate::db::DbThreadRecord,
    parsed: Option<&RolloutParseResult>,
) -> Observed<ModelSpec> {
    let model = parsed
        .and_then(|p| {
            p.requested_model.as_ref().map(|observation| {
                (
                    observation.model.clone(),
                    observation.effort.clone(),
                    observation.source.clone(),
                    observation.observed_at,
                )
            })
        })
        .or_else(|| {
            if thread.model.is_some() || thread.reasoning_effort.is_some() {
                Some((
                    thread.model.clone(),
                    thread.reasoning_effort.clone(),
                    "threads-table".to_string(),
                    thread.updated_at.or(thread.recency_at),
                ))
            } else {
                None
            }
        });

    if let Some((model_name, reasoning_effort, source, observed_at)) = model {
        Observed {
            value: Some(ModelSpec {
                model: model_name,
                reasoning_effort,
            }),
            source: Some(EvidenceSource {
                kind: source.clone(),
                detail: Some(if source == "threads-table" {
                    "persisted requested/resolved config".to_string()
                } else {
                    "from rollout lifecycle".to_string()
                }),
            }),
            observed_at,
            confidence: Confidence::High,
            detail: Some("requested model".to_string()),
        }
    } else {
        Observed::unknown()
    }
}

fn thread_project(thread: &crate::db::DbThreadRecord) -> Option<String> {
    thread.cwd.clone()
}

fn apply_depth_filter(
    threads: Vec<ThreadSnapshot>,
    parent_of: &HashMap<String, String>,
    max_depth: usize,
) -> Vec<ThreadSnapshot> {
    if max_depth == 0 {
        return threads;
    }
    threads
        .into_iter()
        .filter(|thread| {
            let mut current = thread.thread_id.as_str();
            let mut depth = 0usize;
            while let Some(parent) = parent_of.get(current) {
                depth += 1;
                if depth > max_depth {
                    return false;
                }
                current = parent.as_str();
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollout::RolloutParseResult;
    use chrono::TimeZone;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[cfg(windows)]
    use std::sync::Mutex;

    #[cfg(windows)]
    static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn applies_state_filter_matches_aliases() {
        assert!(crate::observer::applies_state(
            &ThreadStateFilter::Running,
            &ThreadState::Running
        ));
        assert!(!crate::observer::applies_state(
            &ThreadStateFilter::Running,
            &ThreadState::Idle
        ));
        assert!(crate::observer::applies_state(
            &ThreadStateFilter::Idle,
            &ThreadState::Idle
        ));
        assert!(crate::observer::applies_state(
            &ThreadStateFilter::Idle,
            &ThreadState::Done
        ));
        assert!(!crate::observer::applies_state(
            &ThreadStateFilter::Idle,
            &ThreadState::Failed
        ));
    }

    #[test]
    fn requested_model_prefers_latest_rollout_event_order() {
        let thread = crate::db::DbThreadRecord {
            id: "t1".into(),
            rollout_path: None,
            created_at: None,
            updated_at: None,
            recency_at: None,
            source: None,
            thread_source: None,
            model: Some("legacy-model".into()),
            reasoning_effort: Some("legacy-effort".into()),
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            cwd: None,
            source_kind: None,
            raw: HashMap::new(),
        };
        let parsed = RolloutParseResult {
            requested_model: Some(crate::rollout::RolloutModelObservation {
                model: Some("tc-model".into()),
                effort: Some("tc-effort".into()),
                source: "rollout.turn_context".into(),
                observed_at: None,
            }),
            ..Default::default()
        };
        let requested = requested_model(&thread, Some(&parsed));
        assert_eq!(
            requested.value.as_ref().unwrap().model.as_ref().unwrap(),
            "tc-model"
        );
        assert_eq!(
            requested
                .value
                .as_ref()
                .unwrap()
                .reasoning_effort
                .as_ref()
                .unwrap(),
            "tc-effort"
        );
        assert_eq!(
            requested.source.as_ref().unwrap().kind,
            "rollout.turn_context".to_string()
        );
        assert_eq!(requested.observed_at, None);
    }

    #[test]
    fn configured_model_missing_remains_unknown() {
        let cfg = crate::config::ConfigContext {
            global: None,
            agents_dir: None,
            home: PathBuf::from("X"),
        };
        let thread = crate::db::DbThreadRecord {
            id: "t1".into(),
            rollout_path: None,
            created_at: None,
            updated_at: None,
            recency_at: None,
            source: None,
            thread_source: None,
            model: None,
            reasoning_effort: None,
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            cwd: None,
            source_kind: None,
            raw: HashMap::new(),
        };
        let observed = configured_model(&thread, &cfg);
        assert_eq!(observed.confidence, Confidence::Low);
        assert!(observed.source.is_some());
    }

    #[test]
    fn requested_model_uses_db_fallback_when_no_rollout_model() {
        let thread = crate::db::DbThreadRecord {
            id: "t2".into(),
            rollout_path: None,
            created_at: None,
            updated_at: Some(Utc.timestamp_opt(1_720_000_020, 0).unwrap()),
            recency_at: None,
            source: None,
            thread_source: None,
            model: Some("db-model".into()),
            reasoning_effort: Some("db-effort".into()),
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            cwd: None,
            source_kind: None,
            raw: HashMap::new(),
        };
        let requested = requested_model(&thread, None);
        assert_eq!(
            requested.value.as_ref().expect("value").model.as_deref(),
            Some("db-model")
        );
        assert_eq!(requested.source.as_ref().unwrap().kind, "threads-table");
        assert_eq!(
            requested.observed_at,
            Some(Utc.timestamp_opt(1_720_000_020, 0).unwrap())
        );
    }

    #[test]
    fn thread_evidence_uses_session_meta_when_db_values_missing() {
        let thread = crate::db::DbThreadRecord {
            id: "t3".into(),
            rollout_path: None,
            created_at: None,
            updated_at: None,
            recency_at: None,
            source: None,
            thread_source: None,
            model: None,
            reasoning_effort: None,
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            cwd: Some("c:\\cwd".into()),
            source_kind: Some("subagent".into()),
            raw: HashMap::new(),
        };
        let rollout = RolloutParseResult {
            canonical_nickname: Some("nick".into()),
            canonical_role: Some("role".into()),
            canonical_parent: Some("parent".into()),
            canonical_agent_path: None,
            canonical_meta_seen: true,
            requested_model: None,
            canonical_meta_timestamp: None,
            warnings: vec![],
            activity: vec![],
            final_state: crate::rollout::RolloutStateHint::Running,
            tail_truncated: false,
        };
        let evidence = thread_evidence(
            &thread,
            Some("parent".into()),
            Some(crate::tree::ParentSource::ThreadSpawnEdge),
            Some(&rollout),
            ThreadState::Running,
            &[],
        );

        assert_eq!(evidence.nickname.value, Some("nick".to_string()));
        assert_eq!(
            evidence.nickname.source.as_ref().unwrap().kind,
            "persisted-session-meta"
        );
        assert_eq!(evidence.role.value, Some("role".to_string()));
        assert_eq!(evidence.parent_thread_id.value, Some("parent".to_string()));
        assert_eq!(
            evidence.parent_thread_id.source.as_ref().unwrap().kind,
            "thread_spawn_edges"
        );
    }

    #[test]
    fn observed_unknown_uses_unknown_source() {
        let serialized = serde_json::to_string(&Observed::<String>::unknown()).expect("serialize");
        assert!(serialized.contains("\"kind\":\"unknown\""));
    }

    #[test]
    fn probe_output_serialization_does_not_leak_sensitive_rollout_values() {
        use crate::model::{ModelSummary, ProbeOutput, ThreadEvidence, ThreadSnapshot};

        let parsed = crate::rollout::parse_rollout_file(
            std::path::Path::new("tests/fixtures/rollout/privacy_activity.jsonl"),
            "thread-secure",
            false,
        );
        let activity = parsed
            .activity
            .iter()
            .map(|act| crate::model::ThreadActivity {
                kind: act.kind.clone(),
                tool_name: act.tool_name.clone(),
                status: act.status.clone(),
                timestamp: act.ts,
            })
            .collect();
        let snapshot = ThreadSnapshot {
            thread_id: "thread-secure".into(),
            nickname: Some("secure".into()),
            role: Some("assistant".into()),
            parent_thread_id: None,
            cwd: Some("C:\\\\tmp".into()),
            source_kind: Some("user".into()),
            children: Vec::new(),
            project: None,
            state: ThreadState::Running,
            state_detail: None,
            model: ModelSummary {
                configured: Observed::unknown(),
                requested: Observed::unknown(),
                effective: Observed::unknown(),
                rerouted_from: None,
                reroute_reason: None,
            },
            created_at: None,
            updated_at: None,
            recency_at: None,
            rollout_path: None,
            warnings: Vec::new(),
            recent_activity: activity,
            evidence: ThreadEvidence::default(),
        };
        let output = ProbeOutput {
            schema_version: "codex-agent-monitor.probe.v1".into(),
            generated_at: Utc::now(),
            codex_home: "home".into(),
            environment: crate::model::ProbeEnvironment {
                os: "test".into(),
                codex_cli_path: None,
                codex_cli_version: None,
            },
            query: crate::model::QueryInfo {
                include_all: false,
                project: None,
                thread: None,
                state: None,
                role: None,
                depth: None,
            },
            warnings: vec![],
            threads: vec![snapshot],
            tree: Vec::new(),
        };
        let json = serde_json::to_string(&output).expect("serialize");
        assert!(!json.contains("SECRET"));
    }

    #[cfg(not(windows))]
    #[test]
    fn paths_match_is_case_sensitive_on_unix() {
        assert!(paths_match("/tmp/Project", "/tmp/Project"));
        assert!(!paths_match("/tmp/Project", "/tmp/project"));
    }

    #[cfg(windows)]
    #[test]
    fn paths_match_windows_handles_verbatim_prefix_and_project_dot() {
        let _guard = TEST_ENV_LOCK.lock().expect("mutex");

        let cwd = std::env::current_dir().expect("cwd");
        let cwd_display = cwd.display().to_string();
        let verbatim_cwd = format!(r"\\?\{}", cwd_display);

        assert!(paths_match(&cwd_display, "."));
        assert!(paths_match(&verbatim_cwd, &cwd_display));
        assert!(paths_match(&cwd_display.to_ascii_lowercase(), &cwd_display));
        assert!(paths_match(&cwd_display.to_ascii_uppercase(), &cwd_display));
        assert!(paths_match(
            r"\\server\share\Project",
            r"\\?\UNC\server\share\Project"
        ));
        assert!(paths_match(
            r"\\server\share\project",
            r"\\?\UNC\SERVER\share\PROJECT"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn find_command_in_path_prefers_pathext_candidates_before_extensionless() {
        let _guard = TEST_ENV_LOCK.lock().expect("mutex");

        let temp = std::env::temp_dir().join("codex-observer-test-cmd-order");
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).expect("mkdir");

        let extless = temp.join("codex");
        fs::write(&extless, "not executable").expect("write");
        let cmd = temp.join("codex.cmd");
        fs::write(&cmd, "@echo codex-cmd").expect("write");

        let path = temp.to_string_lossy().into_owned();
        let candidates =
            find_command_in_path_with_env("codex", Some(&path), Some(".CMD;.EXE;.BAT;.COM"));

        assert!(!candidates.is_empty());
        assert!(candidates[0].ends_with("codex.cmd"));
        assert!(candidates.iter().any(|value| value.ends_with("codex")));
    }

    #[cfg(windows)]
    #[test]
    fn detect_environment_picks_working_pathext_candidate() {
        let _guard = TEST_ENV_LOCK.lock().expect("mutex");

        let temp = std::env::temp_dir().join("codex-observer-test-detect");
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).expect("mkdir");

        let extless = temp.join("codex");
        fs::write(&extless, "not executable").expect("write");
        let cmd = temp.join("codex.cmd");
        fs::write(&cmd, "@echo codex-cli 1.2.3").expect("write");

        let original_path = std::env::var_os("PATH");
        let original_pathext = std::env::var_os("PATHEXT");
        std::env::set_var("PATH", &temp);
        std::env::set_var("PATHEXT", ".CMD;.EXE;.BAT;.COM");

        let detected = detect_environment();

        assert_eq!(
            detected.codex_cli_version.as_deref(),
            Some("codex-cli 1.2.3")
        );
        assert!(detected
            .codex_cli_path
            .as_deref()
            .is_some_and(|path| path.ends_with("codex.cmd")));

        if let Some(path) = original_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        if let Some(pathext) = original_pathext {
            std::env::set_var("PATHEXT", pathext);
        } else {
            std::env::remove_var("PATHEXT");
        }
    }
}
