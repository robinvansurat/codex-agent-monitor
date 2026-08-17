use crate::model::{ModelSpec, Observed, ProbeOutput};

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
            "{}- {}  state={:?}  role={}  parent={}\n",
            prefix,
            t.thread_id,
            t.state,
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
            "{prefix}  state evidence: {:?} ({})\n",
            t.state,
            source_label(&t.evidence.state.source),
        ));
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
            for a in t.recent_activity.iter().take(5) {
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
