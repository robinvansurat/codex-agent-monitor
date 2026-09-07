use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceSource {
    pub kind: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observed<T> {
    pub value: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<EvidenceSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    pub confidence: Confidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl<T> Observed<T> {
    pub fn from_value(
        value: Option<T>,
        kind: &str,
        detail: Option<String>,
        observed_at: Option<DateTime<Utc>>,
    ) -> Self {
        Observed {
            value,
            source: Some(EvidenceSource {
                kind: kind.to_string(),
                detail: detail.clone(),
            }),
            observed_at,
            confidence: Confidence::Medium,
            detail,
        }
    }

    pub fn unknown() -> Self {
        Observed {
            value: None,
            source: Some(EvidenceSource {
                kind: "unknown".to_string(),
                detail: None,
            }),
            observed_at: None,
            confidence: Confidence::Low,
            detail: Some("No local evidence".to_string()),
        }
    }
}

impl<T> Default for Observed<T> {
    fn default() -> Self {
        Self::unknown()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountUsageWindow {
    pub used_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AccountUsage {
    pub primary: Option<AccountUsageWindow>,
    pub secondary: Option<AccountUsageWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSummary {
    pub configured: Observed<ModelSpec>,
    pub requested: Observed<ModelSpec>,
    pub effective: Observed<ModelSpec>,
    #[serde(skip_serializing_if = "is_none_observed")]
    pub rerouted_from: Option<Observed<String>>,
    #[serde(skip_serializing_if = "is_none_observed")]
    pub reroute_reason: Option<Observed<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadState {
    Running,
    Idle,
    Interrupted,
    Failed,
    Done,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadActivity {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadEvidence {
    pub nickname: Observed<String>,
    pub role: Observed<String>,
    pub parent_thread_id: Observed<String>,
    pub state: Observed<ThreadState>,
    pub cwd: Observed<String>,
    pub source_kind: Observed<String>,
    pub latest_activity: Observed<ThreadActivity>,
}

impl Default for ThreadEvidence {
    fn default() -> Self {
        Self {
            nickname: Observed::unknown(),
            role: Observed::unknown(),
            parent_thread_id: Observed::unknown(),
            state: Observed {
                value: Some(ThreadState::Unknown),
                source: Some(EvidenceSource {
                    kind: "thread-state".to_string(),
                    detail: Some("thread not observed in lifecycle".to_string()),
                }),
                observed_at: None,
                confidence: Confidence::Low,
                detail: Some("thread state unavailable".to_string()),
            },
            cwd: Observed::unknown(),
            source_kind: Observed::unknown(),
            latest_activity: Observed::unknown(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub state: ThreadState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_detail: Option<String>,
    pub model: ModelSummary,
    pub token_usage: Observed<TokenUsage>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub recency_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollout_path: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_activity: Vec<ThreadActivity>,
    pub evidence: ThreadEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadTreeNode {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub children: Vec<ThreadTreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeOutput {
    pub schema_version: String,
    pub generated_at: DateTime<Utc>,
    pub codex_home: String,
    pub environment: ProbeEnvironment,
    pub query: QueryInfo,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub account_usage: Observed<AccountUsage>,
    pub threads: Vec<ThreadSnapshot>,
    pub tree: Vec<ThreadTreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeEnvironment {
    pub os: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_cli_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_cli_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryInfo {
    pub include_all: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub depth: Option<usize>,
}

fn is_none_observed<T>(value: &Option<Observed<T>>) -> bool {
    value
        .as_ref()
        .map(|x| x.value.is_none() && x.detail.as_deref() == Some("No local evidence"))
        .unwrap_or(true)
}
