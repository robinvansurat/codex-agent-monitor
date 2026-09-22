use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::process::Command;

use crate::claude::{self, ClaudeThreadRecord};
use crate::cli::{FilterOpts, Provider, ThreadStateFilter};
use crate::config::{
    load_agent_config_value, load_config, resolve_codex_home, sqlite_home, SqliteHome,
};
use crate::db::DbThreadRecord;
use crate::kiro::{self, KiroThreadRecord};
use crate::kiro_usage::{self, UsageClient};
use crate::model::{
    AccountUsage, AccountUsageWindow, ActivitySignal, Confidence, ContextUsage, EvidenceSource,
    KiroAccountUsage, LastTerminalEvent, ModelSpec, ModelSummary, Observed, ProbeEnvironment,
    ProbeOutput, QueryInfo, TaskProgress, ThreadActivity, ThreadEvidence, ThreadSnapshot,
    ThreadState, TokenUsage,
};
use crate::ollama::{self, OllamaServerStatus, OllamaThreadRecord};
use crate::rollout::{
    RolloutParseResult, RolloutStateHint, RolloutTerminalEvent, RolloutTerminalObservation,
};
use crate::runtime::RuntimeOverlay;

#[derive(Debug)]
pub struct Monitor {
    pub codex_home: PathBuf,
    pub kiro_home: PathBuf,
    pub kiro_db: Option<PathBuf>,
    pub ollama_db: Option<PathBuf>,
    pub claude_home: PathBuf,
    pub claude_usage_file: Option<PathBuf>,
    kiro_home_explicit: bool,
    pub provider: Provider,
    pub config: crate::config::ConfigContext,
    environment: ProbeEnvironment,
    rollout_cache: HashMap<PathBuf, CachedRollout>,
    startup_warnings: Vec<String>,
    kiro_usage: KiroUsageState,
    kiro_event_buffer: String,
    ollama_server: OllamaServerStatus,
}

#[derive(Debug)]
struct KiroUsageState {
    client: UsageClient,
    enabled: bool,
    started_at: Option<Instant>,
    receiver: Option<Receiver<Result<Observed<KiroAccountUsage>, String>>>,
    cancel: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
    value: Observed<KiroAccountUsage>,
    last_success: Option<Observed<KiroAccountUsage>>,
    diagnostic: Option<String>,
}

impl KiroUsageState {
    fn new(client: UsageClient, enabled: bool, disabled_reason: Option<String>) -> Self {
        let enabled = enabled && client.is_available();
        let value = if enabled {
            kiro_usage::unknown("Loading native Kiro account credits")
        } else if let Some(reason) = disabled_reason.as_deref() {
            kiro_usage::unknown(reason)
        } else if client.is_available() {
            kiro_usage::unknown("Kiro account lookup disabled")
        } else {
            kiro_usage::unknown("Kiro CLI unavailable")
        };
        Self {
            client,
            enabled,
            started_at: None,
            receiver: None,
            cancel: None,
            worker: None,
            value,
            last_success: None,
            diagnostic: None,
        }
    }

    fn refresh(&mut self, wait: bool) -> (Observed<KiroAccountUsage>, Option<String>) {
        self.poll();
        let due = self
            .started_at
            .is_none_or(|started| started.elapsed() >= kiro_usage::REFRESH_INTERVAL);
        if self.enabled && self.receiver.is_none() && due {
            self.started_at = Some(Instant::now());
            if let Some((receiver, cancel, worker)) = self.client.spawn() {
                self.receiver = Some(receiver);
                self.cancel = Some(cancel);
                self.worker = Some(worker);
            }
            if wait {
                let wait_result = self
                    .receiver
                    .as_ref()
                    .map(|receiver| receiver.recv_timeout(kiro_usage::REQUEST_TIMEOUT));
                if let Some(wait_result) = wait_result {
                    match wait_result {
                        Ok(result) => {
                            self.receiver = None;
                            self.cancel = None;
                            self.join_worker();
                            self.apply(result);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            self.value = self
                                .last_success
                                .clone()
                                .map(|mut value| {
                                    value.confidence = Confidence::Low;
                                    value.detail = Some(
                                        "stale while Kiro lookup is still loading".to_string(),
                                    );
                                    value
                                })
                                .unwrap_or_else(|| {
                                    kiro_usage::unknown("Kiro account lookup is still loading")
                                });
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            self.apply(Err("native lookup unavailable".to_string()));
                        }
                    }
                }
            }
        }
        (self.value.clone(), self.diagnostic.take())
    }

    fn poll(&mut self) {
        let Some(receiver) = self.receiver.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.receiver = None;
                self.cancel = None;
                self.join_worker();
                self.apply(result);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.receiver = None;
                self.cancel = None;
                self.join_worker();
                self.apply(Err("native lookup unavailable".to_string()));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn apply(&mut self, result: Result<Observed<KiroAccountUsage>, String>) {
        match result {
            Ok(value) => {
                self.last_success = Some(value.clone());
                self.value = value;
            }
            Err(error) => {
                self.value = self
                    .last_success
                    .clone()
                    .map(|mut value| {
                        value.confidence = Confidence::Low;
                        value.detail = Some(format!("stale: {error}"));
                        value
                    })
                    .unwrap_or_else(|| kiro_usage::unknown(error.clone()));
                self.diagnostic = Some(format!("Kiro account usage unavailable: {error}"));
            }
        }
    }

    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for KiroUsageState {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        self.join_worker();
    }
}

pub const FRESHNESS_WINDOW_MINUTES: i64 = 15;

#[derive(Debug, Clone)]
struct CachedRollout {
    path_mtime: Option<std::time::SystemTime>,
    path_size: u64,
    parsed: RolloutParseResult,
}

impl Monitor {
    pub fn new(override_home: Option<String>) -> Result<Self> {
        Self::new_with_sources(override_home, None, None, Provider::Codex)
    }

    pub fn new_with_sources(
        override_home: Option<String>,
        kiro_home: Option<String>,
        kiro_db: Option<String>,
        provider: Provider,
    ) -> Result<Self> {
        Self::new_with_sources_and_usage(override_home, kiro_home, kiro_db, provider, None, true)
    }

    pub fn new_with_sources_and_usage(
        override_home: Option<String>,
        kiro_home: Option<String>,
        kiro_db: Option<String>,
        provider: Provider,
        kiro_cli: Option<String>,
        no_kiro_usage: bool,
    ) -> Result<Self> {
        Self::new_with_sources_and_usage_and_ollama(
            override_home,
            kiro_home,
            kiro_db,
            provider,
            kiro_cli,
            no_kiro_usage,
            None,
        )
    }

    pub fn new_with_sources_and_usage_and_ollama(
        override_home: Option<String>,
        kiro_home: Option<String>,
        kiro_db: Option<String>,
        provider: Provider,
        kiro_cli: Option<String>,
        no_kiro_usage: bool,
        ollama_db: Option<String>,
    ) -> Result<Self> {
        let home = resolve_codex_home(override_home.as_deref())?;
        let (resolved_kiro_home, kiro_home_explicit) =
            kiro::resolve_kiro_home(kiro_home.as_deref())?;
        let (resolved_claude_home, _) = claude::resolve_claude_home(None)?;
        let mut startup_warnings = Vec::new();
        let config = if matches!(provider, Provider::Claude | Provider::Kiro) {
            crate::config::ConfigContext {
                global: None,
                agents_dir: None,
                home: home.clone(),
            }
        } else {
            match load_config(&home) {
                Ok(config) => config,
                Err(err) if matches!(provider, Provider::All | Provider::Ollama) => {
                    startup_warnings.push(format!("Codex config unavailable: {err}"));
                    crate::config::ConfigContext {
                        global: None,
                        agents_dir: None,
                        home: home.clone(),
                    }
                }
                Err(err) => return Err(err),
            }
        };
        let kiro_db = kiro_db.map(PathBuf::from).or_else(|| {
            if kiro_home_explicit {
                Some(resolved_kiro_home.join("data.sqlite3"))
            } else {
                None
            }
        });
        let usage_requested = matches!(provider, Provider::Kiro | Provider::All);
        let usage_enabled =
            usage_requested && !no_kiro_usage && (!kiro_home_explicit || kiro_cli.is_some());
        let usage_client = if usage_enabled {
            UsageClient::new(kiro_usage::discover_executable(kiro_cli.as_deref()))
        } else {
            UsageClient::new(None)
        };
        let disabled_reason = if no_kiro_usage && usage_requested {
            Some("Kiro account lookup disabled by --no-kiro-usage".to_string())
        } else if usage_requested && kiro_home_explicit && kiro_cli.is_none() {
            Some("Kiro account lookup disabled for explicit Kiro home".to_string())
        } else {
            None
        };
        Ok(Monitor {
            codex_home: home,
            kiro_home: resolved_kiro_home,
            kiro_db,
            ollama_db: ollama_db.map(PathBuf::from),
            claude_home: resolved_claude_home,
            claude_usage_file: None,
            kiro_home_explicit,
            provider: provider.clone(),
            config,
            environment: detect_environment(!matches!(
                provider,
                Provider::Claude | Provider::Kiro | Provider::Ollama
            )),
            rollout_cache: HashMap::new(),
            startup_warnings,
            kiro_usage: KiroUsageState::new(usage_client, usage_enabled, disabled_reason),
            kiro_event_buffer: String::new(),
            ollama_server: OllamaServerStatus::default(),
        })
    }

    pub fn set_claude_home(&mut self, claude_home: Option<String>) -> Result<()> {
        let (home, _) = claude::resolve_claude_home(claude_home.as_deref())?;
        self.claude_home = home;
        Ok(())
    }

    pub fn set_claude_usage_file(&mut self, path: Option<String>) {
        self.claude_usage_file = path.map(PathBuf::from);
    }

    pub fn probe_snapshot(
        &mut self,
        filters: &FilterOpts,
        runtime: RuntimeOverlay,
        allow_partial_rollout: bool,
    ) -> Result<ProbeOutput> {
        self.probe_snapshot_internal(filters, runtime, allow_partial_rollout, false)
    }

    pub fn probe_snapshot_with_usage_wait(
        &mut self,
        filters: &FilterOpts,
        runtime: RuntimeOverlay,
        allow_partial_rollout: bool,
    ) -> Result<ProbeOutput> {
        self.probe_snapshot_internal(filters, runtime, allow_partial_rollout, true)
    }

    fn probe_snapshot_internal(
        &mut self,
        filters: &FilterOpts,
        runtime: RuntimeOverlay,
        allow_partial_rollout: bool,
        wait_for_kiro_usage: bool,
    ) -> Result<ProbeOutput> {
        let mut db_threads = Vec::new();
        let mut parent_edges = Vec::new();
        let mut warnings = self.startup_warnings.clone();
        let mut rollouts: HashMap<String, RolloutParseResult> = HashMap::new();
        if matches!(
            self.provider,
            Provider::Codex | Provider::All | Provider::Ollama
        ) {
            let sqlite_root = sqlite_home(&self.codex_home, &self.config);
            let db_path = match sqlite_root {
                SqliteHome::Directory(dir) => dir.join("state_5.sqlite"),
                SqliteHome::File(file) => file,
            };
            match crate::db::read_state_db(&db_path) {
                Ok(db) => {
                    warnings.extend(db.warnings);
                    parent_edges = db.parent_edges;
                    db_threads = db.threads;
                    if matches!(self.provider, Provider::Ollama) {
                        // Under the Ollama filter a Codex thread is only in scope
                        // when its persisted model provider is an Ollama backend.
                        db_threads.retain(thread_uses_ollama);
                    }
                }
                Err(err) if matches!(self.provider, Provider::All | Provider::Ollama) => {
                    warnings.push(format!("Codex state unavailable: {err}"));
                }
                Err(err) => return Err(err),
            }
        }
        if matches!(self.provider, Provider::Ollama | Provider::All) {
            let ollama_snapshot = ollama::read_snapshot(self.ollama_db.as_deref());
            warnings.extend(ollama_snapshot.warnings);
            for thread in ollama_snapshot.threads {
                if db_threads.iter().any(|existing| existing.id == thread.id) {
                    continue;
                }
                let id = thread.id.clone();
                rollouts.insert(id.clone(), ollama_rollout(&thread));
                db_threads.push(ollama_db_record(&thread));
            }
            let cli_snapshot = ollama::read_cli_processes();
            warnings.extend(cli_snapshot.warnings);
            for thread in cli_snapshot.threads {
                if db_threads.iter().any(|existing| existing.id == thread.id) {
                    continue;
                }
                let id = thread.id.clone();
                rollouts.insert(id.clone(), ollama_rollout(&thread));
                db_threads.push(ollama_db_record(&thread));
            }
            self.ollama_server = ollama::poll_server();
            for thread in ollama::api_threads(&self.ollama_server, Utc::now()) {
                if db_threads.iter().any(|existing| existing.id == thread.id) {
                    continue;
                }
                let id = thread.id.clone();
                rollouts.insert(id.clone(), ollama_rollout(&thread));
                db_threads.push(ollama_db_record(&thread));
            }
        }
        let mut claude_paths = HashMap::<String, PathBuf>::new();
        let mut claude_account_usage = Observed::<AccountUsage>::unknown();
        if matches!(self.provider, Provider::Claude | Provider::All) {
            claude_account_usage = claude::read_plan_usage(self.claude_usage_file.as_deref());
            let claude_snapshot = claude::read_snapshot(&self.claude_home);
            warnings.extend(claude_snapshot.warnings);
            for thread in claude_snapshot.threads {
                let id = thread.id.clone();
                if db_threads.iter().any(|existing| existing.id == id) {
                    continue;
                }
                claude_paths.extend(thread.path.clone().map(|path| (id.clone(), path)));
                rollouts.insert(id.clone(), claude_rollout(&thread));
                db_threads.push(claude_db_record(&thread));
            }
        }
        let mut kiro_paths = HashMap::<String, PathBuf>::new();
        let mut kiro_context_usage = HashMap::<String, Observed<ContextUsage>>::new();
        let mut kiro_task_progress = HashMap::<String, Observed<TaskProgress>>::new();
        if matches!(self.provider, Provider::Kiro | Provider::All) {
            let kiro_snapshot = kiro::read_snapshot_with_buffer(
                &self.kiro_home,
                self.kiro_db.as_deref(),
                self.kiro_home_explicit,
                &mut self.kiro_event_buffer,
            );
            warnings.extend(kiro_snapshot.warnings);
            for kiro_thread in kiro_snapshot.threads {
                let id = kiro_thread.id.clone();
                if db_threads.iter().any(|thread| thread.id == id) {
                    continue;
                }
                kiro_paths.extend(kiro_thread.path.clone().map(|path| (id.clone(), path)));
                if let Some(context_usage) = kiro_thread.context_usage.clone() {
                    kiro_context_usage.insert(
                        id.clone(),
                        Observed {
                            value: Some(context_usage),
                            source: Some(EvidenceSource {
                                kind: "kiro.session.context_usage".to_string(),
                                detail: Some(
                                    "persisted current-context percentage and model window"
                                        .to_string(),
                                ),
                            }),
                            observed_at: kiro_thread.context_usage_at,
                            confidence: Confidence::Medium,
                            detail: Some(
                                "current context occupancy; approximate tokens are derived from the persisted percentage and window, not cumulative token usage"
                                    .to_string(),
                            ),
                        },
                    );
                }
                if kiro_thread.task_progress.is_some() || kiro_thread.task_progress_detail.is_some()
                {
                    let has_progress = kiro_thread.task_progress.is_some();
                    kiro_task_progress.insert(
                        id.clone(),
                        Observed {
                            value: kiro_thread.task_progress.clone(),
                            source: Some(EvidenceSource {
                                kind: "kiro.tasks".to_string(),
                                detail: Some(
                                    if has_progress {
                                        "native Kiro task store"
                                    } else {
                                        "Kiro task-plan availability"
                                    }
                                    .to_string(),
                                ),
                            }),
                            observed_at: kiro_thread.task_progress_at,
                            confidence: if has_progress {
                                Confidence::Medium
                            } else {
                                Confidence::Low
                            },
                            detail: kiro_thread.task_progress_detail.clone().or_else(|| {
                                Some(
                                    "numeric task ids and statuses only; task text excluded"
                                        .to_string(),
                                )
                            }),
                        },
                    );
                }
                rollouts.insert(id.clone(), kiro_rollout(&kiro_thread));
                db_threads.push(kiro_db_record(&kiro_thread));
            }
        }
        let all_threads = db_threads;

        let candidate_threads: Vec<&crate::db::DbThreadRecord> = all_threads
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
                    .is_none_or(|needle| thread_id_matches(&thread.id, needle))
            })
            .map(|thread| thread.id.clone())
            .collect();

        for thread_id in &selected_ids {
            if let Some(thread) = all_threads
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

        let hints = crate::tree::collect_parent_hints(&all_threads, &rollouts);
        let (edges, parent_sources) =
            crate::tree::build_parent_edges_with_sources(parent_edges, &hints);
        let mut parent_of = HashMap::new();
        for (parent, child) in &edges {
            parent_of.insert(child.clone(), parent.clone());
        }

        let mut thread_rows = Vec::new();
        for thread in candidate_threads {
            if !selected_ids.contains(&thread.id) {
                continue;
            }
            let parsed_rollout = rollouts.get(&thread.id);
            let hint_state = parsed_rollout.map(|r| r.final_state);
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
                if !thread_id_matches(&thread.id, thread_filter) {
                    continue;
                }
            }

            let requested = requested_model(thread, rollouts.get(&thread.id));
            // Codex config defaults describe Codex threads only; an Ollama row has
            // no Codex configuration behind it.
            let configured = if thread
                .source_kind
                .as_deref()
                .is_some_and(|kind| kind.starts_with("ollama_"))
            {
                Observed::unknown()
            } else {
                configured_model(thread, &self.config)
            };
            let is_kiro = thread
                .source_kind
                .as_deref()
                .is_some_and(|kind| kind.starts_with("kiro_"));
            let mut effective = Observed::unknown();
            if !is_kiro {
                if let Some(value) = runtime.effective_model.get(&thread.id) {
                    effective = value.clone();
                }
            }
            let mut activity = Vec::new();
            if let Some(parsed) = parsed_rollout {
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

            let thread_state = state;
            let last_terminal_event =
                last_terminal_event_observation(parsed_rollout, thread.source_kind.as_deref());
            let activity_signal = activity_signal_observation(thread, parsed_rollout);

            let mut children = Vec::new();
            for (child, parent) in &parent_of {
                if parent == &thread.id {
                    children.push(child.clone());
                }
            }
            children.sort_unstable();
            let parent_thread_id = parent_of.get(&thread.id).cloned();
            let parent_source = parent_sources.get(&thread.id).copied();
            let mut evidence = thread_evidence(
                thread,
                parent_thread_id.clone(),
                parent_source,
                rollouts.get(&thread.id),
                thread_state.clone(),
                &activity,
            );
            let lifecycle_at = parsed_rollout.and_then(|rollout| rollout.final_state_at);
            evidence.state.observed_at = lifecycle_at;
            evidence.state.confidence = state_confidence(&thread_state, lifecycle_at);
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
                last_terminal_event,
                activity_signal,
                model: ModelSummary {
                    configured,
                    requested,
                    effective,
                    rerouted_from: if is_kiro {
                        None
                    } else {
                        runtime.rerouted_from.get(&thread.id).cloned()
                    },
                    reroute_reason: if is_kiro {
                        None
                    } else {
                        runtime.reroute_reason.get(&thread.id).cloned()
                    },
                },
                token_usage: token_usage_observation(rollouts.get(&thread.id)),
                context_usage: kiro_context_usage
                    .get(&thread.id)
                    .cloned()
                    .unwrap_or_else(Observed::unknown),
                task_progress: kiro_task_progress
                    .get(&thread.id)
                    .cloned()
                    .unwrap_or_else(Observed::unknown),
                created_at: thread.created_at,
                updated_at: thread.updated_at,
                recency_at: thread.recency_at,
                rollout_path: thread
                    .rollout_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .or_else(|| kiro_paths.get(&thread.id).map(|p| p.display().to_string()))
                    .or_else(|| {
                        claude_paths
                            .get(&thread.id)
                            .map(|p| p.display().to_string())
                    }),
                warnings: Vec::new(),
                recent_activity: activity,
                evidence,
            });
        }

        thread_rows.sort_by(compare_recency_then_id);
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

        let account_usage = account_usage_observation(&rollouts);
        let (kiro_account_usage, kiro_usage_warning) = self.kiro_usage.refresh(wait_for_kiro_usage);

        let mut combined_warnings = Vec::new();
        combined_warnings.extend(warnings);
        combined_warnings.extend(runtime.warnings);
        if let Some(warning) = kiro_usage_warning {
            combined_warnings.push(warning);
        }
        Ok(ProbeOutput {
            schema_version: "codex-agent-monitor.probe.v2".to_string(),
            generated_at: Utc::now(),
            codex_home: self.codex_home.display().to_string(),
            environment: self.environment.clone(),
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
                provider: Some(
                    match self.provider {
                        Provider::Codex => "codex",
                        Provider::Claude => "claude",
                        Provider::Kiro => "kiro",
                        Provider::Ollama => "ollama",
                        Provider::All => "all",
                    }
                    .to_string(),
                ),
            },
            warnings: combined_warnings,
            account_usage,
            claude_account_usage,
            kiro_account_usage,
            ollama_server: self.ollama_server.clone(),
            threads: thread_rows,
            tree,
        })
    }
}

fn kiro_db_record(thread: &KiroThreadRecord) -> crate::db::DbThreadRecord {
    crate::db::DbThreadRecord {
        id: thread.id.clone(),
        rollout_path: None,
        created_at: thread.created_at,
        updated_at: thread.updated_at,
        recency_at: thread.recency_at,
        source: None,
        thread_source: None,
        model: None,
        reasoning_effort: None,
        agent_nickname: thread.nickname.clone(),
        agent_role: thread.role.clone(),
        agent_path: None,
        cwd: thread.cwd.clone(),
        source_kind: Some(thread.source_kind.clone()),
        raw: HashMap::new(),
    }
}

fn ollama_db_record(thread: &OllamaThreadRecord) -> crate::db::DbThreadRecord {
    crate::db::DbThreadRecord {
        id: thread.id.clone(),
        rollout_path: None,
        created_at: thread.created_at,
        updated_at: thread.updated_at,
        recency_at: thread.recency_at,
        source: None,
        thread_source: None,
        model: thread.model.clone(),
        reasoning_effort: None,
        agent_nickname: thread.nickname.clone(),
        agent_role: None,
        agent_path: None,
        cwd: None,
        source_kind: Some(thread.source_kind.clone()),
        raw: HashMap::new(),
    }
}

fn claude_db_record(thread: &ClaudeThreadRecord) -> crate::db::DbThreadRecord {
    crate::db::DbThreadRecord {
        id: thread.id.clone(),
        rollout_path: None,
        created_at: thread.created_at,
        updated_at: thread.updated_at,
        recency_at: thread.recency_at,
        source: None,
        thread_source: None,
        model: thread.model.clone(),
        reasoning_effort: thread.effort.clone(),
        agent_nickname: thread.nickname.clone(),
        agent_role: thread.role.clone(),
        agent_path: None,
        cwd: thread.cwd.clone(),
        source_kind: Some(thread.source_kind.clone()),
        raw: HashMap::new(),
    }
}

fn claude_rollout(thread: &ClaudeThreadRecord) -> RolloutParseResult {
    let final_state = match thread.state {
        ThreadState::Running => RolloutStateHint::Running,
        ThreadState::Idle => RolloutStateHint::TurnCompleted,
        ThreadState::Unknown => RolloutStateHint::Unknown,
    };
    let terminal = thread
        .terminal
        .map(|(event, observed_at)| RolloutTerminalObservation { event, observed_at });
    let latest_activity_at = thread.activity.iter().filter_map(|item| item.ts).max();
    RolloutParseResult {
        warnings: thread.warnings.clone(),
        requested_model: if thread.model.is_some() || thread.effort.is_some() {
            Some(crate::rollout::RolloutModelObservation {
                model: thread.model.clone(),
                effort: thread.effort.clone(),
                source: "claude_code.transcript".to_string(),
                observed_at: thread.updated_at,
            })
        } else {
            None
        },
        token_usage: thread.token_usage.clone(),
        activity: thread.activity.clone(),
        final_state,
        last_terminal_event: terminal,
        final_state_at: thread.lifecycle_at,
        latest_lifecycle_at: thread.lifecycle_at,
        latest_activity_at,
        ..RolloutParseResult::default()
    }
}

fn ollama_rollout(thread: &OllamaThreadRecord) -> RolloutParseResult {
    let final_state = match thread.state {
        ThreadState::Running => RolloutStateHint::Running,
        ThreadState::Idle => RolloutStateHint::TurnCompleted,
        ThreadState::Unknown => RolloutStateHint::Unknown,
    };
    RolloutParseResult {
        final_state,
        final_state_at: thread.activity_at,
        latest_lifecycle_at: thread.activity_at,
        latest_activity_at: thread.activity_at,
        ..RolloutParseResult::default()
    }
}

fn kiro_rollout(thread: &KiroThreadRecord) -> RolloutParseResult {
    let final_state = match thread.state {
        ThreadState::Running => RolloutStateHint::Running,
        ThreadState::Idle => match thread.terminal.as_ref().map(|(event, _)| event) {
            Some(RolloutTerminalEvent::Failed) => RolloutStateHint::Failed,
            Some(RolloutTerminalEvent::Interrupted) => RolloutStateHint::Interrupted,
            _ => RolloutStateHint::TurnCompleted,
        },
        ThreadState::Unknown => RolloutStateHint::Unknown,
    };
    let terminal = thread
        .terminal
        .map(|(event, observed_at)| RolloutTerminalObservation { event, observed_at });
    let latest_activity_at = thread.activity.iter().filter_map(|item| item.ts).max();
    RolloutParseResult {
        warnings: thread.warnings.clone(),
        requested_model: if thread.model.is_some() || thread.effort.is_some() {
            Some(crate::rollout::RolloutModelObservation {
                model: thread.model.clone(),
                effort: thread.effort.clone(),
                source: format!("{}.session", thread.source_kind),
                observed_at: thread.updated_at,
            })
        } else {
            None
        },
        activity: thread.activity.clone(),
        final_state,
        last_terminal_event: terminal,
        final_state_at: thread.lifecycle_at,
        latest_lifecycle_at: thread.lifecycle_at,
        latest_activity_at,
        ..RolloutParseResult::default()
    }
}

fn thread_id_matches(id: &str, filter: &str) -> bool {
    id == filter
        || id.strip_prefix("kiro:").is_some_and(|raw| raw == filter)
        || id.strip_prefix("ollama:").is_some_and(|raw| raw == filter)
        || id.strip_prefix("claude:").is_some_and(|raw| raw == filter)
}

fn account_usage_observation(
    rollouts: &HashMap<String, RolloutParseResult>,
) -> Observed<AccountUsage> {
    let Some(observation) = rollouts
        .values()
        .filter_map(|rollout| rollout.account_usage.as_ref())
        .max_by_key(|observation| observation.observed_at)
    else {
        return Observed::unknown();
    };

    let convert = |window: &crate::rollout::RolloutAccountUsageWindow| AccountUsageWindow {
        used_percent: window.used_percent,
        window_minutes: window.window_minutes,
        resets_at: window.resets_at,
    };
    Observed {
        value: Some(AccountUsage {
            primary: observation.primary.as_ref().map(convert),
            secondary: observation.secondary.as_ref().map(convert),
        }),
        source: Some(EvidenceSource {
            kind: observation.source.clone(),
            detail: Some("latest timestamped account rate limit observation".to_string()),
        }),
        observed_at: Some(observation.observed_at),
        confidence: Confidence::High,
        detail: Some("account allowance from monitored rollout evidence".to_string()),
    }
}

fn token_usage_observation(parsed: Option<&RolloutParseResult>) -> Observed<TokenUsage> {
    let Some(observation) = parsed.and_then(|rollout| rollout.token_usage.as_ref()) else {
        return Observed::unknown();
    };
    Observed {
        value: Some(TokenUsage {
            input_tokens: observation.input_tokens,
            cached_input_tokens: observation.cached_input_tokens,
            cache_write_input_tokens: observation.cache_write_input_tokens,
            output_tokens: observation.output_tokens,
            reasoning_output_tokens: observation.reasoning_output_tokens,
            total_tokens: observation.total_tokens,
            context_window: observation.context_window,
        }),
        source: Some(EvidenceSource {
            kind: observation.source.clone(),
            detail: Some("latest valid cumulative token_count observation".to_string()),
        }),
        observed_at: observation.observed_at,
        confidence: Confidence::High,
        detail: Some("cumulative task token usage; account credits unavailable".to_string()),
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

fn detect_environment(include_codex: bool) -> ProbeEnvironment {
    let os = std::env::consts::OS.to_string();
    let mut codex_cli_path = None;
    let mut codex_cli_version = None;
    for path in include_codex
        .then(|| find_command_in_path("codex"))
        .into_iter()
        .flatten()
    {
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

fn compare_recency_then_id(a: &ThreadSnapshot, b: &ThreadSnapshot) -> std::cmp::Ordering {
    compare_recency_values(a.recency_at, &a.thread_id, b.recency_at, &b.thread_id)
}

fn compare_recency_values(
    a_recency: Option<DateTime<Utc>>,
    a_id: &str,
    b_recency: Option<DateTime<Utc>>,
    b_id: &str,
) -> std::cmp::Ordering {
    match (a_recency, b_recency) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a_id.cmp(b_id),
    }
    .then_with(|| a_id.cmp(b_id))
}

fn applies_state(filter: &ThreadStateFilter, state: &ThreadState) -> bool {
    match filter {
        ThreadStateFilter::Running => matches!(state, ThreadState::Running),
        ThreadStateFilter::Idle => matches!(state, ThreadState::Idle),
        ThreadStateFilter::Unknown => matches!(state, ThreadState::Unknown),
    }
}

fn map_state(hint: Option<RolloutStateHint>) -> ThreadState {
    match hint.unwrap_or(RolloutStateHint::Unknown) {
        RolloutStateHint::Running => ThreadState::Running,
        RolloutStateHint::Idle => ThreadState::Idle,
        RolloutStateHint::TurnCompleted
        | RolloutStateHint::Interrupted
        | RolloutStateHint::Failed => ThreadState::Idle,
        RolloutStateHint::Unknown => ThreadState::Unknown,
    }
}

fn state_confidence(state: &ThreadState, lifecycle_at: Option<DateTime<Utc>>) -> Confidence {
    state_confidence_at(state, lifecycle_at, Utc::now())
}

fn state_confidence_at(
    state: &ThreadState,
    lifecycle_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Confidence {
    if matches!(state, ThreadState::Unknown) {
        return Confidence::Low;
    }
    match lifecycle_at {
        Some(timestamp) if timestamp >= now - ChronoDuration::minutes(FRESHNESS_WINDOW_MINUTES) => {
            Confidence::Medium
        }
        _ => Confidence::Low,
    }
}

fn last_terminal_event_observation(
    rollout: Option<&RolloutParseResult>,
    source_kind: Option<&str>,
) -> Observed<LastTerminalEvent> {
    let Some(rollout) = rollout else {
        return Observed::unknown();
    };
    let Some(observation) = rollout.last_terminal_event else {
        return Observed::unknown();
    };
    let value = match observation.event {
        RolloutTerminalEvent::Completed => LastTerminalEvent::Completed,
        RolloutTerminalEvent::Failed => LastTerminalEvent::Failed,
        RolloutTerminalEvent::Interrupted => LastTerminalEvent::Interrupted,
    };
    let observed_at = observation.observed_at;
    Observed {
        value: Some(value),
        source: Some(EvidenceSource {
            kind: if source_kind.is_some_and(|kind| kind.starts_with("kiro_")) {
                "kiro.lifecycle"
            } else {
                "rollout.lifecycle"
            }
            .to_string(),
            detail: Some("latest terminal lifecycle event".to_string()),
        }),
        observed_at,
        confidence: Confidence::Medium,
        detail: Some("terminal result does not change current state".to_string()),
    }
}

fn activity_signal_observation(
    thread: &DbThreadRecord,
    rollout: Option<&RolloutParseResult>,
) -> Observed<ActivitySignal> {
    activity_signal_observation_at(thread, rollout, Utc::now())
}

fn activity_signal_observation_at(
    thread: &DbThreadRecord,
    rollout: Option<&RolloutParseResult>,
    now: DateTime<Utc>,
) -> Observed<ActivitySignal> {
    let is_kiro = thread
        .source_kind
        .as_deref()
        .is_some_and(|kind| kind.starts_with("kiro_"));
    let rollout_timestamp = rollout.and_then(|value| {
        [value.latest_activity_at, value.latest_lifecycle_at]
            .into_iter()
            .flatten()
            .max()
    });
    let fallback_source = if is_kiro {
        "kiro.persisted-state"
    } else {
        "threads-table"
    };
    let (timestamp, source) = rollout_timestamp
        .map(|value| (Some(value), "rollout.activity"))
        .unwrap_or_else(|| {
            (
                thread
                    .recency_at
                    .or(thread.updated_at)
                    .or(thread.created_at),
                fallback_source,
            )
        });
    let Some(timestamp) = timestamp else {
        return Observed {
            value: Some(ActivitySignal::Unknown),
            source: Some(EvidenceSource {
                kind: "unknown".to_string(),
                detail: Some("no persisted activity timestamp".to_string()),
            }),
            observed_at: None,
            confidence: Confidence::Low,
            detail: Some("activity freshness unavailable".to_string()),
        };
    };
    let recent = timestamp >= now - ChronoDuration::minutes(FRESHNESS_WINDOW_MINUTES);
    let source = if is_kiro && source == "rollout.activity" {
        "kiro.activity"
    } else {
        source
    };
    Observed {
        value: Some(if recent {
            ActivitySignal::Recent
        } else {
            ActivitySignal::Stale
        }),
        source: Some(EvidenceSource {
            kind: source.to_string(),
            detail: Some(
                if source == "kiro.persisted-state" {
                    "latest Kiro session or task-store timestamp"
                } else {
                    "latest available activity timestamp"
                }
                .to_string(),
            ),
        }),
        observed_at: Some(timestamp),
        confidence: if matches!(source, "threads-table" | "kiro.persisted-state") {
            Confidence::Low
        } else {
            Confidence::Medium
        },
        detail: Some(if recent {
            "activity is within freshness window".to_string()
        } else {
            "activity is older than freshness window".to_string()
        }),
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
    let kiro_source = thread
        .source_kind
        .as_deref()
        .is_some_and(|kind| kind.starts_with("kiro_"));
    let ollama_api_source = thread.source_kind.as_deref() == Some("ollama_api");
    let session_source = if kiro_source {
        "kiro.session"
    } else if ollama_api_source {
        "ollama.api.ps"
    } else {
        "persisted-session"
    };

    let latest_activity = activity.last().cloned().map(|activity| {
        let confidence = if activity.timestamp.is_some() {
            Confidence::Medium
        } else {
            Confidence::Low
        };
        Observed {
            value: Some(activity),
            source: Some(EvidenceSource {
                kind: session_source.to_string(),
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
                    if kiro_source {
                        "kiro.session"
                    } else if thread.agent_nickname.is_some() {
                        "threads-table"
                    } else {
                        "persisted-session-meta"
                    },
                    Some(if kiro_source {
                        "from Kiro session".to_string()
                    } else if thread.agent_nickname.is_some() {
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
                    if kiro_source {
                        "kiro.session"
                    } else if thread.agent_role.is_some() {
                        "threads-table"
                    } else {
                        "persisted-session-meta"
                    },
                    Some(if kiro_source {
                        "from Kiro session".to_string()
                    } else if thread.agent_role.is_some() {
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
                kind: session_source.to_string(),
                detail: Some(if kiro_source {
                    "derived from Kiro lifecycle".to_string()
                } else if ollama_api_source {
                    "model resident in the local Ollama server".to_string()
                } else {
                    "derived from rollout lifecycle".to_string()
                }),
            }),
            observed_at: None,
            confidence: if state_is_unknown {
                Confidence::Low
            } else {
                Confidence::Medium
            },
            detail: Some(if ollama_api_source {
                "model loaded in the local Ollama server; keep-alive residency indicates recent use, not a confirmed in-flight request"
                    .to_string()
            } else {
                "thread state".to_string()
            }),
        },
        cwd: cwd
            .map(|value| {
                Observed::from_value(
                    Some(value),
                    if kiro_source {
                        "kiro.session"
                    } else {
                        "threads-table"
                    },
                    Some(if kiro_source {
                        "from Kiro session".to_string()
                    } else {
                        "from threads table".to_string()
                    }),
                    None,
                )
            })
            .unwrap_or_else(Observed::unknown),
        source_kind: source_kind
            .map(|value| {
                Observed::from_value(
                    Some(value),
                    if kiro_source {
                        "kiro.session"
                    } else {
                        "threads-table"
                    },
                    Some(if kiro_source {
                        "from Kiro session".to_string()
                    } else {
                        "from threads table".to_string()
                    }),
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
    if thread
        .source_kind
        .as_deref()
        .is_some_and(|kind| kind.starts_with("kiro_"))
    {
        return Observed {
            value: None,
            source: Some(EvidenceSource {
                kind: "kiro.session".to_string(),
                detail: Some("Kiro configured model is not available".to_string()),
            }),
            observed_at: None,
            confidence: Confidence::Low,
            detail: Some("configured model unknown".to_string()),
        };
    }
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
        let model_name = if thread_uses_ollama(thread) {
            model_name.map(|model| format!("{model} via Ollama"))
        } else {
            model_name
        };
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

fn thread_uses_ollama(thread: &crate::db::DbThreadRecord) -> bool {
    thread
        .raw
        .iter()
        .any(|(key, value)| key.eq_ignore_ascii_case("model_provider") && value_is_ollama(value))
}

fn value_is_ollama(value: &serde_json::Value) -> bool {
    value
        .as_str()
        .is_some_and(|provider| provider.to_ascii_lowercase().starts_with("ollama"))
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
            let mut visited = HashSet::new();
            while let Some(parent) = parent_of.get(current) {
                if !visited.insert(current) {
                    return false;
                }
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

    fn test_thread() -> DbThreadRecord {
        DbThreadRecord {
            id: "thread".into(),
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
        }
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).unwrap()
    }

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
        assert!(!crate::observer::applies_state(
            &ThreadStateFilter::Idle,
            &ThreadState::Unknown
        ));
    }

    #[test]
    fn activity_signal_uses_latest_rollout_timestamp_and_evidence_confidence() {
        let now = at(1_720_000_000);
        let thread = test_thread();
        let rollout = RolloutParseResult {
            latest_activity_at: Some(at(1_719_999_000)),
            latest_lifecycle_at: Some(at(1_719_999_500)),
            ..Default::default()
        };
        let recent = activity_signal_observation_at(&thread, Some(&rollout), now);
        assert_eq!(recent.value, Some(ActivitySignal::Recent));
        assert_eq!(recent.observed_at, Some(at(1_719_999_500)));
        assert_eq!(recent.confidence, Confidence::Medium);

        let stale_rollout = RolloutParseResult {
            latest_activity_at: Some(at(1_719_998_000)),
            ..Default::default()
        };
        let stale = activity_signal_observation_at(&thread, Some(&stale_rollout), now);
        assert_eq!(stale.value, Some(ActivitySignal::Stale));
        assert_eq!(stale.confidence, Confidence::Medium);

        let mut fallback_thread = test_thread();
        fallback_thread.updated_at = Some(at(1_719_999_500));
        let fallback = activity_signal_observation_at(&fallback_thread, None, now);
        assert_eq!(fallback.value, Some(ActivitySignal::Recent));
        assert_eq!(fallback.source.unwrap().kind, "threads-table");
        assert_eq!(fallback.confidence, Confidence::Low);

        let unknown = activity_signal_observation_at(&thread, None, now);
        assert_eq!(unknown.value, Some(ActivitySignal::Unknown));
        assert_eq!(unknown.confidence, Confidence::Low);
    }

    #[test]
    fn state_confidence_is_deterministic_for_fresh_stale_and_missing_evidence() {
        let now = at(1_720_000_000);
        assert_eq!(
            state_confidence_at(&ThreadState::Running, Some(at(1_719_999_500)), now),
            Confidence::Medium
        );
        assert_eq!(
            state_confidence_at(&ThreadState::Running, Some(at(1_719_998_000)), now),
            Confidence::Low
        );
        assert_eq!(
            state_confidence_at(&ThreadState::Running, None, now),
            Confidence::Low
        );
        assert_eq!(
            state_confidence_at(&ThreadState::Unknown, Some(now), now),
            Confidence::Low
        );
    }

    #[test]
    fn historical_terminal_evidence_keeps_medium_confidence() {
        let observed_at = at(1_700_000_000);
        let rollout = RolloutParseResult {
            last_terminal_event: Some(crate::rollout::RolloutTerminalObservation {
                event: RolloutTerminalEvent::Failed,
                observed_at: Some(observed_at),
            }),
            ..Default::default()
        };
        let observation = last_terminal_event_observation(Some(&rollout), None);
        assert_eq!(observation.value, Some(LastTerminalEvent::Failed));
        assert_eq!(observation.observed_at, Some(observed_at));
        assert_eq!(observation.confidence, Confidence::Medium);
    }

    fn minimal_snapshot(id: &str) -> ThreadSnapshot {
        ThreadSnapshot {
            thread_id: id.to_string(),
            nickname: None,
            role: None,
            parent_thread_id: None,
            cwd: None,
            source_kind: None,
            children: Vec::new(),
            project: None,
            state: ThreadState::Unknown,
            last_terminal_event: Observed::unknown(),
            activity_signal: Observed::unknown(),
            model: ModelSummary {
                configured: Observed::unknown(),
                requested: Observed::unknown(),
                effective: Observed::unknown(),
                rerouted_from: None,
                reroute_reason: None,
            },
            token_usage: Observed::unknown(),
            context_usage: Observed::unknown(),
            task_progress: Observed::unknown(),
            created_at: None,
            updated_at: None,
            recency_at: None,
            rollout_path: None,
            warnings: Vec::new(),
            recent_activity: Vec::new(),
            evidence: ThreadEvidence::default(),
        }
    }

    #[test]
    fn depth_filter_excludes_rows_reaching_a_cycle() {
        let parent_of = HashMap::from([
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
            ("c".to_string(), "a".to_string()),
        ]);
        let rows = ["a", "b", "c", "d"]
            .into_iter()
            .map(minimal_snapshot)
            .collect();
        let filtered = apply_depth_filter(rows, &parent_of, 10);
        assert_eq!(
            filtered
                .iter()
                .map(|row| row.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["d"]
        );
    }

    #[test]
    fn recency_order_is_deterministic_for_equal_and_missing_values() {
        let now = at(1_720_000_000);
        assert_eq!(
            compare_recency_values(Some(now), "b", Some(now), "a"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_recency_values(None, "b", None, "a"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_recency_values(Some(now), "a", None, "z"),
            std::cmp::Ordering::Less
        );
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
    fn token_usage_observation_preserves_breakdown_and_evidence() {
        let parsed = RolloutParseResult {
            token_usage: Some(crate::rollout::RolloutTokenUsageObservation {
                input_tokens: Some(10),
                cached_input_tokens: Some(2),
                cache_write_input_tokens: None,
                output_tokens: Some(4),
                reasoning_output_tokens: Some(1),
                total_tokens: Some(15),
                context_window: Some(128_000),
                source: "rollout.token_count".to_string(),
                observed_at: Some(Utc.timestamp_opt(1_720_000_100, 0).unwrap()),
            }),
            ..Default::default()
        };
        let observed = token_usage_observation(Some(&parsed));
        let usage = observed.value.expect("token usage");
        assert_eq!(usage.total_tokens, Some(15));
        assert_eq!(usage.cache_write_input_tokens, None);
        assert_eq!(usage.context_window, Some(128_000));
        assert_eq!(observed.source.unwrap().kind, "rollout.token_count");
        assert_eq!(observed.confidence, Confidence::High);
    }

    #[test]
    fn account_usage_observation_selects_latest_rollout_timestamp() {
        let older = RolloutParseResult {
            account_usage: Some(crate::rollout::RolloutAccountUsageObservation {
                primary: Some(crate::rollout::RolloutAccountUsageWindow {
                    used_percent: Some(10.0),
                    window_minutes: Some(300),
                    resets_at: None,
                }),
                secondary: None,
                source: "rollout.rate_limits".into(),
                observed_at: Utc.timestamp_opt(1_720_000_100, 0).unwrap(),
            }),
            ..Default::default()
        };
        let newer = RolloutParseResult {
            account_usage: Some(crate::rollout::RolloutAccountUsageObservation {
                primary: Some(crate::rollout::RolloutAccountUsageWindow {
                    used_percent: Some(44.0),
                    window_minutes: Some(10_080),
                    resets_at: None,
                }),
                secondary: None,
                source: "rollout.rate_limits".into(),
                observed_at: Utc.timestamp_opt(1_720_000_200, 0).unwrap(),
            }),
            ..Default::default()
        };
        let rollouts = HashMap::from([("older".into(), older), ("newer".into(), newer)]);
        let observed = account_usage_observation(&rollouts);
        assert_eq!(
            observed.observed_at,
            Some(Utc.timestamp_opt(1_720_000_200, 0).unwrap())
        );
        assert_eq!(
            observed.value.unwrap().primary.unwrap().used_percent,
            Some(44.0)
        );
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
    fn ollama_launch_provider_is_annotated_without_changing_source() {
        let mut thread = test_thread();
        thread.model = Some("llama3.2".into());
        thread
            .raw
            .insert("model_provider".into(), serde_json::json!("ollama-launch"));
        let observed = requested_model(&thread, None);
        assert_eq!(
            observed.value.unwrap().model.as_deref(),
            Some("llama3.2 via Ollama")
        );
        assert!(thread_uses_ollama(&thread));
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
            token_usage: None,
            account_usage: None,
            canonical_meta_timestamp: None,
            warnings: vec![],
            activity: vec![],
            final_state: crate::rollout::RolloutStateHint::Running,
            last_terminal_event: None,
            final_state_at: None,
            latest_lifecycle_at: None,
            latest_activity_at: None,
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
            last_terminal_event: Observed::unknown(),
            activity_signal: Observed::unknown(),
            model: ModelSummary {
                configured: Observed::unknown(),
                requested: Observed::unknown(),
                effective: Observed::unknown(),
                rerouted_from: None,
                reroute_reason: None,
            },
            token_usage: Observed::unknown(),
            context_usage: Observed::unknown(),
            task_progress: Observed::unknown(),
            created_at: None,
            updated_at: None,
            recency_at: None,
            rollout_path: None,
            warnings: Vec::new(),
            recent_activity: activity,
            evidence: ThreadEvidence::default(),
        };
        let output = ProbeOutput {
            schema_version: "codex-agent-monitor.probe.v2".into(),
            generated_at: Utc::now(),
            codex_home: "home".into(),
            environment: crate::model::ProbeEnvironment {
                os: "test".into(),
                codex_cli_path: None,
                codex_cli_version: None,
            },
            query: crate::model::QueryInfo {
                provider: None,
                include_all: false,
                project: None,
                thread: None,
                state: None,
                role: None,
                depth: None,
            },
            warnings: vec![],
            account_usage: Observed::unknown(),
            kiro_account_usage: Observed::unknown(),
            claude_account_usage: Observed::unknown(),
            ollama_server: OllamaServerStatus::default(),
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
