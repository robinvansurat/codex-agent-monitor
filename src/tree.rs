use std::collections::{HashMap, HashSet};

use crate::db::DbThreadRecord;
use crate::model::ThreadTreeNode;
use crate::rollout::{pick_key_paths, RolloutParseResult};

#[derive(Default)]
pub struct RawParentHints {
    pub source_hints: HashMap<String, String>,
    pub rollout_hints: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentSource {
    ThreadSpawnEdge,
    SourceHint,
    RolloutHint,
}
impl RawParentHints {
    pub fn new() -> Self {
        Self {
            source_hints: HashMap::new(),
            rollout_hints: HashMap::new(),
        }
    }
}

pub fn collect_parent_hints(
    threads: &[DbThreadRecord],
    rollouts: &HashMap<String, RolloutParseResult>,
) -> RawParentHints {
    let mut hints = RawParentHints::new();
    for thread in threads {
        if let Some(source) = &thread.source {
            if let Some(parent) = extract_parent_from_source(source) {
                if parent != thread.id {
                    hints.source_hints.insert(thread.id.clone(), parent);
                }
            }
        }
        if let Some(parent) = rollouts
            .get(&thread.id)
            .and_then(|p| p.canonical_parent.clone())
        {
            if parent != thread.id {
                hints.rollout_hints.insert(thread.id.clone(), parent);
            }
        }
    }
    hints
}

pub fn build_parent_edges(
    explicit_edges: Vec<(String, String)>,
    hints: &RawParentHints,
) -> Vec<(String, String)> {
    build_parent_edges_with_sources(explicit_edges, hints).0
}

pub fn build_parent_edges_with_sources(
    explicit_edges: Vec<(String, String)>,
    hints: &RawParentHints,
) -> (Vec<(String, String)>, HashMap<String, ParentSource>) {
    let mut explicit_by_child = HashMap::<String, String>::new();
    let mut parent_sources = HashMap::<String, ParentSource>::new();
    let mut edges = Vec::new();

    let mut explicit_edges = explicit_edges;
    explicit_edges.sort_unstable_by(|(parent_a, child_a), (parent_b, child_b)| {
        child_a.cmp(child_b).then_with(|| parent_a.cmp(parent_b))
    });
    for (parent, child) in explicit_edges {
        if parent == child {
            continue;
        }
        if explicit_by_child.contains_key(&child)
            || creates_cycle(&child, &parent, &explicit_by_child)
        {
            continue;
        }
        explicit_by_child.insert(child.clone(), parent.clone());
        edges.push((parent.clone(), child.clone()));
        parent_sources.insert(child.clone(), ParentSource::ThreadSpawnEdge);
    }

    let mut source_hints = hints.source_hints.iter().collect::<Vec<_>>();
    source_hints.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for (child, parent) in source_hints {
        if explicit_by_child.contains_key(child) {
            continue;
        }
        if !creates_cycle(child, parent, &explicit_by_child) {
            explicit_by_child.insert((*child).clone(), (*parent).clone());
            edges.push(((*parent).clone(), (*child).clone()));
            parent_sources
                .entry((*child).clone())
                .or_insert(ParentSource::SourceHint);
        }
    }

    let mut rollout_hints = hints.rollout_hints.iter().collect::<Vec<_>>();
    rollout_hints.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for (child, parent) in rollout_hints {
        if explicit_by_child.contains_key(child) {
            continue;
        }
        if !creates_cycle(child, parent, &explicit_by_child) {
            explicit_by_child.insert((*child).clone(), (*parent).clone());
            edges.push(((*parent).clone(), (*child).clone()));
            parent_sources
                .entry((*child).clone())
                .or_insert(ParentSource::RolloutHint);
        }
    }

    edges.sort_unstable();
    edges.dedup();
    (edges, parent_sources)
}

pub fn build_thread_tree(thread_ids: &[String], edges: &[(String, String)]) -> Vec<ThreadTreeNode> {
    let ids: HashSet<String> = thread_ids.iter().cloned().collect();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();

    let mut ordered_edges = edges.to_vec();
    ordered_edges.sort_unstable_by(|(parent_a, child_a), (parent_b, child_b)| {
        child_a.cmp(child_b).then_with(|| parent_a.cmp(parent_b))
    });
    for (parent, child) in &ordered_edges {
        if !ids.contains(parent) || !ids.contains(child) {
            continue;
        }
        if parent == child {
            continue;
        }
        if creates_cycle(child, parent, &parent_of) {
            continue;
        }
        if parent_of.contains_key(child) {
            continue;
        }
        parent_of.insert(child.clone(), parent.clone());
    }

    for (child, parent) in parent_of.iter() {
        children
            .entry(parent.clone())
            .or_default()
            .push(child.clone());
    }
    for list in children.values_mut() {
        list.sort_unstable();
    }

    let mut roots: Vec<String> = thread_ids
        .iter()
        .filter(|id| !parent_of.contains_key(*id))
        .cloned()
        .collect();
    roots.sort_unstable();

    let mut nodes = Vec::new();
    for root in roots {
        nodes.push(build_node(root, &children));
    }
    nodes
}

fn build_node(id: String, children: &HashMap<String, Vec<String>>) -> ThreadTreeNode {
    build_node_with_visited(id, children, &mut HashSet::new())
}

fn build_node_with_visited(
    id: String,
    children: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
) -> ThreadTreeNode {
    if !visited.insert(id.clone()) {
        return ThreadTreeNode {
            thread_id: id,
            parent: None,
            children: Vec::new(),
        };
    }
    let nodes = children.get(&id).cloned().unwrap_or_default();
    let child_nodes = nodes
        .into_iter()
        .map(|child| build_node_with_visited(child, children, visited))
        .collect();
    ThreadTreeNode {
        thread_id: id,
        parent: None,
        children: child_nodes,
    }
}

fn creates_cycle(
    candidate_child: &str,
    candidate_parent: &str,
    parent_of: &HashMap<String, String>,
) -> bool {
    let mut current = Some(candidate_parent.to_string());
    let mut visited = HashSet::new();
    while let Some(current_id) = current {
        if current_id == candidate_child {
            return true;
        }
        if !visited.insert(current_id.clone()) {
            return true;
        }
        current = parent_of.get(&current_id).cloned();
    }
    false
}

fn extract_parent_from_source(source: &serde_json::Value) -> Option<String> {
    pick_key_paths(
        source,
        &[
            &["subagent", "thread_spawn", "parent_thread_id"],
            &["subAgent", "threadSpawn", "parentThreadId"],
            &["thread_spawn", "parent_thread_id"],
            &["threadSpawn", "parentThreadId"],
        ],
    )
    .and_then(serde_json::Value::as_str)
    .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_tree_prevents_cycles_and_keeps_orphans() {
        let threads = vec![
            crate::db::DbThreadRecord {
                id: "a".into(),
                rollout_path: None,
                created_at: None,
                updated_at: None,
                recency_at: None,
                source: Some(json!({"subagent":{"thread_spawn":{"parent_thread_id":"c"}}})),
                thread_source: None,
                model: None,
                reasoning_effort: None,
                agent_nickname: None,
                agent_role: None,
                agent_path: None,
                cwd: None,
                source_kind: None,
                raw: Default::default(),
            },
            crate::db::DbThreadRecord {
                id: "b".into(),
                rollout_path: None,
                created_at: None,
                updated_at: None,
                recency_at: None,
                source: Some(json!({"subagent":{"thread_spawn":{"parent_thread_id":"a"}}})),
                thread_source: None,
                model: None,
                reasoning_effort: None,
                agent_nickname: None,
                agent_role: None,
                agent_path: None,
                cwd: None,
                source_kind: None,
                raw: Default::default(),
            },
            crate::db::DbThreadRecord {
                id: "c".into(),
                rollout_path: None,
                created_at: None,
                updated_at: None,
                recency_at: None,
                source: Some(json!({"subagent":{"thread_spawn":{"parent_thread_id":"b"}}})),
                thread_source: None,
                model: None,
                reasoning_effort: None,
                agent_nickname: None,
                agent_role: None,
                agent_path: None,
                cwd: None,
                source_kind: None,
                raw: Default::default(),
            },
        ];
        let ids: Vec<String> = threads.iter().map(|x| x.id.clone()).collect();
        let edges =
            build_parent_edges(Vec::new(), &collect_parent_hints(&threads, &HashMap::new()));
        let tree = build_thread_tree(&ids, &edges);
        assert_eq!(edges.len(), 2);
        fn collect_ids(node: &ThreadTreeNode, out: &mut Vec<String>) {
            out.push(node.thread_id.clone());
            for child in &node.children {
                collect_ids(child, out);
            }
        }
        let mut ids_seen = Vec::new();
        for node in &tree {
            collect_ids(node, &mut ids_seen);
        }
        assert_eq!(ids_seen.len(), 3);
        assert!(tree.len() <= 1);
    }

    #[test]
    fn parent_hint_prefers_thread_spawn_edges_over_none() {
        let threads = vec![
            crate::db::DbThreadRecord {
                id: "child".into(),
                rollout_path: None,
                created_at: None,
                updated_at: None,
                recency_at: None,
                source: Some(
                    json!({"subagent":{"thread_spawn":{"parent_thread_id":"source-parent"}}}),
                ),
                thread_source: None,
                model: None,
                reasoning_effort: None,
                agent_nickname: None,
                agent_role: None,
                agent_path: None,
                cwd: None,
                source_kind: None,
                raw: Default::default(),
            },
            crate::db::DbThreadRecord {
                id: "source-parent".into(),
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
                raw: Default::default(),
            },
        ];
        let mut rollout_map = HashMap::new();
        rollout_map.insert(
            "child".into(),
            RolloutParseResult {
                canonical_parent: Some("rollout-parent".into()),
                ..Default::default()
            },
        );
        let hints = collect_parent_hints(&threads, &rollout_map);
        let edges = build_parent_edges(Vec::new(), &hints);
        assert!(edges.contains(&("source-parent".into(), "child".into())));
    }

    #[test]
    fn explicit_parent_wins_over_hints() {
        let explicit = vec![("p1".to_string(), "child".to_string())];
        let mut rollout_map = HashMap::new();
        rollout_map.insert(
            "child".into(),
            RolloutParseResult {
                canonical_parent: Some("rollout".into()),
                ..Default::default()
            },
        );
        let mut source = HashMap::new();
        source.insert(
            "child".to_string(),
            json!({
                "thread_spawn":{"parent_thread_id":"source-parent"}
            }),
        );
        let db_thread = crate::db::DbThreadRecord {
            id: "child".into(),
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
            raw: Default::default(),
        };
        let mut child = db_thread;
        child.source = source.remove("child");
        let hints = collect_parent_hints(&[child], &rollout_map);
        let edges = build_parent_edges(explicit, &hints);
        assert_eq!(edges, vec![("p1".to_string(), "child".to_string())]);
    }

    #[test]
    fn explicit_edges_are_cycle_safe_and_duplicate_children_are_deterministic() {
        let hints = RawParentHints::new();
        let edges = build_parent_edges(
            vec![
                ("z-parent".into(), "child".into()),
                ("a-parent".into(), "child".into()),
                ("child".into(), "z-parent".into()),
                ("z-parent".into(), "z-parent".into()),
            ],
            &hints,
        );
        assert_eq!(
            edges,
            vec![
                ("a-parent".to_string(), "child".to_string()),
                ("child".to_string(), "z-parent".to_string()),
            ]
        );
        assert_eq!(
            build_parent_edges(
                vec![("a".into(), "b".into()), ("b".into(), "a".into())],
                &hints,
            ),
            vec![("b".to_string(), "a".to_string())]
        );
    }
}
