use crate::model::{
    ActivitySignal, ContextUsage, KiroAccountUsage, LastTerminalEvent, ModelSpec, Observed,
    ProbeOutput, TaskProgress, ThreadState, TokenUsage,
};
use chrono::Utc;

pub fn render_human(summary: &ProbeOutput) -> String {
    let mut out = String::new();
    out.push_str(&format!("schema: {}\n", summary.schema_version));
    out.push_str(&format!("generated: {}\n", summary.generated_at));
    out.push_str(&format!("codex home: {}\n", summary.codex_home));
    out.push_str(&format!(
        "monitor: {}\n",
        match summary.query.provider.as_deref() {
            Some("claude") => "Claude Agent Monitor",
            Some("kiro") => "Kiro Agent Monitor",
            Some("ollama") => "Ollama Agent Monitor",
            Some("all") => "Codex + Claude + Kiro (Ollama)",
            _ => "Codex Agent Monitor",
        }
    ));
    if matches!(
        summary.query.provider.as_deref(),
        Some("codex") | Some("all") | None
    ) {
        out.push_str(&format!(
            "codex usage: {}\n",
            codex_usage_label(&summary.account_usage)
        ));
    }
    if matches!(
        summary.query.provider.as_deref(),
        Some("claude") | Some("all")
    ) {
        let (tokens, sessions) = claude_token_summary(&summary.threads);
        out.push_str(&format!(
            "claude usage: {}; tokens: {}; sessions: {}\n",
            codex_usage_label(&summary.claude_account_usage),
            format_count(tokens),
            sessions
        ));
    }
    if matches!(
        summary.query.provider.as_deref(),
        Some("ollama") | Some("all")
    ) {
        let status = if summary.ollama_server.available {
            "available"
        } else {
            "unavailable"
        };
        let models = if summary.ollama_server.loaded_models.is_empty() {
            "none".to_string()
        } else {
            summary.ollama_server.loaded_models.join(", ")
        };
        out.push_str(&format!("ollama api: {status}; loaded models: {models}\n"));
    }
    if matches!(
        summary.query.provider.as_deref(),
        Some("kiro") | Some("all")
    ) {
        out.push_str(&format!(
            "kiro credits: {}\n",
            kiro_usage_label(&summary.kiro_account_usage)
        ));
    }
    if !summary.warnings.is_empty() {
        out.push_str("warnings:\n");
        for w in &summary.warnings {
            out.push_str(&format!(" - {w}\n"));
        }
    }
    for node in &summary.tree {
        render_node(node, summary, 0, &mut out);
    }
    if summary.tree.is_empty() {
        out.push_str("No matching threads found.\n");
    }
    out
}

fn render_node(
    node: &crate::model::ThreadTreeNode,
    snapshot: &crate::model::ProbeOutput,
    depth: usize,
    out: &mut String,
) {
    let prefix = "  ".repeat(depth);
    if let Some(t) = snapshot
        .threads
        .iter()
        .find(|x| x.thread_id == node.thread_id)
    {
        out.push_str(&format!(
            "{}- [{}] {}  state={}  role={}  parent={}\n",
            prefix,
            provider_label(t.source_kind.as_deref()),
            t.thread_id,
            state_label(&t.state),
            t.role.clone().unwrap_or_else(|| "-".to_string()),
            t.parent_thread_id
                .clone()
                .unwrap_or_else(|| "-".to_string()),
        ));
        out.push_str(&format!(
            "{}  identity: nickname={} (source={}) role={} (source={})\n",
            prefix,
            t.nickname.clone().unwrap_or_else(|| "-".to_string()),
            source_label(&t.evidence.nickname.source),
            t.role.clone().unwrap_or_else(|| "-".to_string()),
            source_label(&t.evidence.role.source),
        ));
        out.push_str(&format!(
            "{prefix}  state evidence: {} ({})\n",
            state_label(&t.state),
            source_label(&t.evidence.state.source),
        ));
        out.push_str(&format!(
            "{prefix}  activity signal: {} ({})\n",
            activity_signal_label(&t.activity_signal),
            source_label(&t.activity_signal.source),
        ));
        if let Some(event) = t.last_terminal_event.value.as_ref() {
            out.push_str(&format!(
                "{prefix}  last terminal event: {} ({})\n",
                terminal_event_label(event),
                source_label(&t.last_terminal_event.source),
            ));
        }
        out.push_str(&format!(
            "{}  cwd: {} (source={}) source_kind: {} (source={})\n",
            prefix,
            t.cwd.clone().unwrap_or_else(|| "-".to_string()),
            source_label(&t.evidence.cwd.source),
            friendly_source_kind(t.source_kind.as_deref()),
            source_label(&t.evidence.source_kind.source),
        ));
        out.push_str(&format!(
            "{}  configured model: {} (source={})\n",
            prefix,
            observed_model_label(&t.model.configured),
            source_label(&t.model.configured.source),
        ));
        out.push_str(&format!(
            "{}  requested model: {} (source={})\n",
            prefix,
            observed_model_label(&t.model.requested),
            source_label(&t.model.requested.source),
        ));
        out.push_str(&format!(
            "{}  effective model: {} (source={})\n",
            prefix,
            observed_model_label(&t.model.effective),
            source_label(&t.model.effective.source),
        ));
        out.push_str(&format!(
            "{}  token usage: {} (source={})\n",
            prefix,
            observed_token_usage_label(&t.token_usage),
            source_label(&t.token_usage.source),
        ));
        if t.context_usage.value.is_some() {
            out.push_str(&format!(
                "{}  context usage: {} (source={})\n",
                prefix,
                observed_context_usage_label(&t.context_usage),
                source_label(&t.context_usage.source),
            ));
        }
        if let Some(progress) = t.task_progress.value.as_ref() {
            out.push_str(&format!(
                "{}  task progress: {} (source={})\n",
                prefix,
                task_progress_label(progress),
                source_label(&t.task_progress.source),
            ));
            if !progress.tasks.is_empty() {
                let tasks = progress
                    .tasks
                    .iter()
                    .map(|task| format!("#{} {}", task.id, task.status.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("{}  tasks: {tasks}\n", prefix));
            }
        } else if let Some(detail) = t
            .task_progress
            .detail
            .as_deref()
            .filter(|detail| *detail != "No local evidence")
        {
            out.push_str(&format!(
                "{}  task progress: unavailable ({detail})\n",
                prefix
            ));
        }
        if let Some(eff) = &t.model.rerouted_from {
            out.push_str(&format!(
                "{}  rerouted from: {} (source={})\n",
                prefix,
                observed_string_label(eff),
                source_label(&eff.source),
            ));
        }
        if let Some(reason) = &t.model.reroute_reason {
            out.push_str(&format!(
                "{}  reroute reason: {} (source={})\n",
                prefix,
                observed_string_label(reason),
                source_label(&reason.source),
            ));
        }
        if !t.recent_activity.is_empty() {
            out.push_str(&format!("{}  recent activity:\n", prefix));
            for a in latest_activity_for_human(&t.recent_activity) {
                let tool = a.tool_name.clone().unwrap_or_else(|| "-".to_string());
                let status = a.status.clone().unwrap_or_else(|| "-".to_string());
                out.push_str(&format!("{}    {} {} {}\n", prefix, a.kind, tool, status));
            }
        }
    }
    for c in &node.children {
        render_node(c, snapshot, depth + 1, out);
    }
}

fn provider_label(source_kind: Option<&str>) -> &'static str {
    if matches!(source_kind, Some("ollama_desktop") | Some("ollama_cli")) {
        "Ollama"
    } else if matches!(source_kind, Some("claude_code")) {
        "Claude"
    } else if source_kind.is_some_and(|kind| kind.starts_with("kiro_")) {
        "Kiro"
    } else {
        "Codex"
    }
}

fn friendly_source_kind(source_kind: Option<&str>) -> &'static str {
    match source_kind {
        Some("ollama_desktop") => "Ollama Desktop",
        Some("ollama_cli") => "Ollama CLI",
        Some("claude_code") => "Claude Code",
        Some("kiro_cli") => "Kiro CLI",
        Some("kiro_acp") => "Kiro ACP worker",
        Some(_) => "Codex",
        None => "unknown",
    }
}

fn codex_usage_label(observed: &Observed<crate::model::AccountUsage>) -> String {
    let Some(value) = observed.value.as_ref() else {
        return "unavailable".to_string();
    };
    let mut windows = Vec::new();
    for window in [value.primary.as_ref(), value.secondary.as_ref()]
        .into_iter()
        .flatten()
    {
        let remaining = if window.resets_at.is_some_and(|reset| reset <= Utc::now()) {
            "awaiting update".to_string()
        } else {
            window
                .used_percent
                .map(|used| format!("{:.0}%", (100.0 - used).clamp(0.0, 100.0)))
                .unwrap_or_else(|| "unavailable".to_string())
        };
        let duration = window
            .window_minutes
            .map(codex_duration_label)
            .unwrap_or_else(|| "unknown window".to_string());
        windows.push(format!("{duration} {remaining}"));
    }
    if windows.is_empty() {
        "unavailable".to_string()
    } else {
        windows.join(" · ")
    }
}

fn codex_duration_label(minutes: u64) -> String {
    match minutes {
        300 => "5h".to_string(),
        10_080 => "Weekly".to_string(),
        minutes if minutes % (24 * 60) == 0 => format!("{}d", minutes / (24 * 60)),
        minutes if minutes % 60 == 0 => format!("{}h", minutes / 60),
        minutes => format!("{}m", minutes),
    }
}

fn kiro_usage_label(observed: &Observed<KiroAccountUsage>) -> String {
    let Some(value) = observed.value.as_ref() else {
        return observed
            .detail
            .clone()
            .unwrap_or_else(|| "unavailable".to_string());
    };
    let Some(plan) = value.plan_credits.as_ref() else {
        return "unavailable".to_string();
    };
    let stale = if observed.confidence == crate::model::Confidence::Low {
        "(stale) "
    } else {
        ""
    };
    let mut label = format!(
        "{stale}{:.2} left / {:.2} plan credits",
        plan.remaining, plan.total
    );
    if let Some(reset) = value.billing_cycle_reset.as_deref() {
        label.push_str(&format!(" (reset {reset})"));
    }
    for bonus in &value.bonus_credits {
        let Some(days) = bonus.days_until_expiry else {
            continue;
        };
        let expiry = if days == 0 {
            "expires today".to_string()
        } else {
            format!("expires in {days}d")
        };
        label.push_str(&format!(
            "; bonus {} {:.2} remaining ({expiry})",
            bonus.name.as_deref().unwrap_or("credits"),
            bonus.remaining
        ));
    }
    for add_on in &value.add_on_credits {
        if add_on.is_active == Some(true) {
            label.push_str(&format!("; add-on {:.2} remaining", add_on.remaining));
        }
    }
    label
}

fn source_label(source: &Option<crate::model::EvidenceSource>) -> &str {
    source
        .as_ref()
        .map(|x| x.kind.as_str())
        .unwrap_or("unknown")
}

fn latest_activity_for_human(
    activities: &[crate::model::ThreadActivity],
) -> Vec<&crate::model::ThreadActivity> {
    activities.iter().rev().take(5).rev().collect()
}

fn state_label(state: &ThreadState) -> &'static str {
    match state {
        ThreadState::Running => "RUNNING",
        ThreadState::Idle => "IDLE",
        ThreadState::Unknown => "UNKNOWN",
    }
}

fn activity_signal_label(signal: &Observed<ActivitySignal>) -> &'static str {
    match signal.value {
        Some(ActivitySignal::Recent) => "recent",
        Some(ActivitySignal::Stale) => "stale",
        Some(ActivitySignal::Unknown) | None => "unknown",
    }
}

fn terminal_event_label(event: &LastTerminalEvent) -> &'static str {
    match event {
        LastTerminalEvent::Completed => "completed",
        LastTerminalEvent::Failed => "failed",
        LastTerminalEvent::Interrupted => "interrupted",
    }
}

fn task_progress_label(progress: &TaskProgress) -> String {
    format!(
        "{}/{} completed",
        progress.completed_count(),
        progress.tasks.len()
    )
}

fn observed_model_label(value: &Observed<ModelSpec>) -> String {
    let details = value
        .value
        .as_ref()
        .map(|model| {
            let name = model.model.clone().unwrap_or_else(|| "<none>".to_string());
            let effort = model
                .reasoning_effort
                .clone()
                .unwrap_or_else(|| "<none>".to_string());
            format!("{name} / {effort}")
        })
        .unwrap_or_else(|| "<none>".to_string());
    details.to_string()
}

fn observed_string_label(value: &Observed<String>) -> String {
    value.value.clone().unwrap_or_else(|| "<none>".to_string())
}

fn observed_token_usage_label(value: &Observed<TokenUsage>) -> String {
    let Some(usage) = value.value.as_ref() else {
        return "total=<none> input=<none> cached_input=<none> cache_write_input=<none> output=<none> reasoning_output=<none> context_window=<none>".to_string();
    };
    format!(
        "total={} input={} cached_input={} cache_write_input={} output={} reasoning_output={} context_window={}",
        token_count_label(usage.total_tokens),
        token_count_label(usage.input_tokens),
        token_count_label(usage.cached_input_tokens),
        token_count_label(usage.cache_write_input_tokens),
        token_count_label(usage.output_tokens),
        token_count_label(usage.reasoning_output_tokens),
        token_count_label(usage.context_window),
    )
}

fn observed_context_usage_label(value: &Observed<ContextUsage>) -> String {
    let Some(usage) = value.value.as_ref() else {
        return "unavailable".to_string();
    };
    format!(
        "{:.1}% used (approximately {} / {} tokens) [current context, not cumulative]",
        usage.used_percent,
        format_count(usage.used_tokens_approx),
        format_count(usage.context_window_tokens),
    )
}

fn token_count_label(value: Option<u64>) -> String {
    value
        .map(format_count)
        .unwrap_or_else(|| "<none>".to_string())
}

/// Claude Code persists no account allowance, so the summary reports observed
/// tokens across the listed sessions rather than a quota.
fn claude_token_summary(threads: &[crate::model::ThreadSnapshot]) -> (u64, usize) {
    threads
        .iter()
        .filter(|thread| thread.source_kind.as_deref() == Some("claude_code"))
        .fold((0_u64, 0_usize), |(tokens, sessions), thread| {
            let total = thread
                .token_usage
                .value
                .as_ref()
                .and_then(|usage| usage.total_tokens)
                .unwrap_or(0);
            (tokens.saturating_add(total), sessions + 1)
        })
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confidence, EvidenceSource};
    use chrono::{TimeZone, Utc};

    #[test]
    fn human_output_renders_token_usage_breakdown_and_source() {
        let observed = Observed {
            value: Some(TokenUsage {
                input_tokens: Some(1_234),
                cached_input_tokens: Some(22),
                cache_write_input_tokens: None,
                output_tokens: Some(567),
                reasoning_output_tokens: Some(8),
                total_tokens: Some(1_831),
                context_window: Some(128_000),
            }),
            source: Some(EvidenceSource {
                kind: "rollout.token_count".to_string(),
                detail: None,
            }),
            observed_at: None,
            confidence: Confidence::High,
            detail: None,
        };
        let label = observed_token_usage_label(&observed);
        assert!(label.contains("total=1,831"));
        assert!(label.contains("cache_write_input=<none>"));
        assert!(label.contains("context_window=128,000"));
        assert!(!label.contains("credit"));
        let json = serde_json::to_string(&observed).expect("serialize token usage");
        assert!(json.contains("\"total_tokens\":1831"));
        assert!(json.contains("\"cache_write_input_tokens\":null"));
    }

    #[test]
    fn unknown_token_usage_is_explicit_in_human_label_and_json() {
        let observed = Observed::<TokenUsage>::unknown();
        let label = observed_token_usage_label(&observed);
        assert!(label.contains("total=<none>"));
        let json = serde_json::to_string(&observed).expect("serialize unknown token usage");
        assert!(json.contains("\"value\":null"));
        assert!(json.contains("\"kind\":\"unknown\""));
    }

    #[test]
    fn human_activity_keeps_latest_stream_entries_in_stream_order() {
        let timestamps = [
            Some(0),
            Some(3),
            Some(2),
            None,
            Some(1),
            Some(7),
            None,
            Some(6),
        ];
        let activities = timestamps
            .into_iter()
            .enumerate()
            .map(|(index, timestamp)| crate::model::ThreadActivity {
                kind: format!("event-{index}"),
                tool_name: None,
                status: None,
                timestamp: timestamp
                    .map(|value| Utc.timestamp_opt(1_720_000_000 + value, 0).unwrap()),
            })
            .collect::<Vec<_>>();
        let latest = latest_activity_for_human(&activities);
        assert_eq!(latest.len(), 5);
        assert_eq!(
            latest
                .iter()
                .map(|entry| entry.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["event-3", "event-4", "event-5", "event-6", "event-7"]
        );
    }
}
