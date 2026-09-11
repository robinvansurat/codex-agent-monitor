use crate::model::{
    ActivitySignal, LastTerminalEvent, ModelSpec, Observed, ProbeOutput, ThreadState, TokenUsage,
};

pub fn render_human(summary: &ProbeOutput) -> String {
    let mut out = String::new();
    out.push_str(&format!("schema: {}\n", summary.schema_version));
    out.push_str(&format!("generated: {}\n", summary.generated_at));
    out.push_str(&format!("codex home: {}\n", summary.codex_home));
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
            "{}- {}  state={}  role={}  parent={}\n",
            prefix,
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
            t.source_kind.clone().unwrap_or_else(|| "-".to_string()),
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

fn token_count_label(value: Option<u64>) -> String {
    value
        .map(format_count)
        .unwrap_or_else(|| "<none>".to_string())
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
