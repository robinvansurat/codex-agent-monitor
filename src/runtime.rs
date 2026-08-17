use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use serde_json::Value;

use crate::model::{Confidence, EvidenceSource, ModelSpec, Observed};

#[derive(Debug, Default, Clone)]
pub struct RuntimeOverlay {
    pub effective_model: HashMap<String, Observed<ModelSpec>>,
    pub rerouted_from: HashMap<String, Observed<String>>,
    pub reroute_reason: HashMap<String, Observed<String>>,
    pub warnings: Vec<String>,
}

impl RuntimeOverlay {
    pub fn from_source(path_or_stdin: Option<&str>) -> Self {
        let mut overlay = RuntimeOverlay::default();
        let Some(source) = path_or_stdin else {
            return overlay;
        };
        if source == "-" {
            let stdin = io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            let mut buf = String::new();
            while let Ok(size) = reader.read_line(&mut buf) {
                if size == 0 {
                    break;
                }
                overlay.apply_line(buf.trim_end());
                buf.clear();
            }
            return overlay;
        }
        if let Ok(content) = fs_read_to_string(source) {
            for line in content.lines() {
                overlay.apply_line(line);
            }
            return overlay;
        }
        overlay
            .warnings
            .push("runtime-events source unavailable".to_string());
        overlay
    }

    pub fn apply_line(&mut self, line: &str) {
        apply_overlay_line(self, line);
    }
}

fn apply_overlay_line(overlay: &mut RuntimeOverlay, line: &str) {
    let text = line.trim();
    if text.is_empty() {
        return;
    }
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            overlay
                .warnings
                .push("malformed runtime event line ignored".to_string());
            return;
        }
    };

    let params = msg.get("params");
    let payload = msg.get("payload");
    let source = params.or(payload).unwrap_or(&msg);

    let method = source
        .get("method")
        .or_else(|| msg.get("method"))
        .or_else(|| msg.get("type"))
        .or_else(|| source.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(method, "model/rerouted" | "modelRerouted") {
        overlay
            .warnings
            .push(format!("unsupported runtime notification: {method}"));
        return;
    }
    let thread_id = lookup_field(
        &msg,
        payload.unwrap_or(source),
        &["threadId", "thread_id", "thread"],
    );
    let turn_id = lookup_field(&msg, payload.unwrap_or(source), &["turnId", "turn_id"]);
    let from_model = lookup_field(
        &msg,
        payload.unwrap_or(source),
        &["fromModel", "from_model"],
    );
    let to_model = lookup_field(&msg, payload.unwrap_or(source), &["toModel", "to_model"]);
    let reason = lookup_field(&msg, payload.unwrap_or(source), &["reason"]);
    let detail = turn_id.as_ref().map(|v| format!("turn_id={v}"));

    if let Some(id) = thread_id {
        if let Some(to) = to_model {
            let source = EvidenceSource {
                kind: "runtime-event:model/rerouted".to_string(),
                detail: detail.clone(),
            };
            overlay.effective_model.insert(
                id.clone(),
                Observed {
                    value: Some(ModelSpec {
                        model: Some(to),
                        reasoning_effort: None,
                    }),
                    source: Some(source),
                    observed_at: Some(chrono::Utc::now()),
                    confidence: Confidence::High,
                    detail: Some("runtime reroute".to_string()),
                },
            );
        }
        if let Some(from) = from_model {
            overlay.rerouted_from.insert(
                id.clone(),
                Observed {
                    value: Some(from),
                    source: Some(EvidenceSource {
                        kind: "runtime-event:model/rerouted".to_string(),
                        detail: detail.clone(),
                    }),
                    observed_at: Some(chrono::Utc::now()),
                    confidence: Confidence::Medium,
                    detail: Some("runtime reroute".to_string()),
                },
            );
        }
        if let Some(reason) = reason {
            overlay.reroute_reason.insert(
                id.clone(),
                Observed {
                    value: Some(reason),
                    source: Some(EvidenceSource {
                        kind: "runtime-event:model/rerouted".to_string(),
                        detail,
                    }),
                    observed_at: Some(chrono::Utc::now()),
                    confidence: Confidence::Medium,
                    detail: Some("runtime reroute".to_string()),
                },
            );
        }
    }
}

fn lookup_ci(value: &Value, key: &str) -> Option<String> {
    value.as_object().and_then(|obj| {
        if let Some(v) = obj.get(key) {
            return v.as_str().map(ToString::to_string);
        }
        obj.iter().find_map(|(k, v)| {
            if k.eq_ignore_ascii_case(key) {
                v.as_str().map(ToString::to_string)
            } else {
                None
            }
        })
    })
}

fn lookup_field(message: &Value, fallback: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        lookup_ci(message, k).or_else(|| {
            fallback
                .get(*k)
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
    })
}

fn fs_read_to_string(path: &str) -> io::Result<String> {
    let mut file = File::open(Path::new(path))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_overlay_from_source_file_parses_reroute() {
        let overlay = RuntimeOverlay::from_source(Some("tests/fixtures/rollout/sample.jsonl"));
        let model = overlay
            .effective_model
            .get("thread-root")
            .expect("effective model");
        assert_eq!(
            model.value.as_ref().and_then(|m| m.model.as_ref()).unwrap(),
            "gpt-4o"
        );
        assert_eq!(
            overlay
                .rerouted_from
                .get("thread-root")
                .and_then(|v| v.value.as_ref())
                .unwrap(),
            "gpt-4"
        );
        assert!(overlay.reroute_reason.contains_key("thread-root"));
    }

    #[test]
    fn runtime_overlay_supports_stdin_events() {
        // We don't execute stdin here, but this helper keeps coverage for parse entrypoint shape by feeding
        // lines through apply_line directly.
        let mut overlay = RuntimeOverlay::default();
        overlay.apply_line(
            r#"{"method":"model/rerouted","threadId":"t1","fromModel":"a","toModel":"b"}"#,
        );
        let model = overlay.effective_model.get("t1").expect("effective model");
        assert_eq!(
            model.value.as_ref().and_then(|m| m.model.as_ref()).unwrap(),
            "b"
        );
    }

    #[test]
    fn runtime_overlay_supports_snake_case_fields() {
        let mut overlay = RuntimeOverlay::default();
        overlay.apply_line(
            r#"{"method":"model/rerouted","thread_id":"t2","from_model":"old","to_model":"new","reason":"policy","turn_id":"turn-2"}"#,
        );
        let model = overlay.effective_model.get("t2").expect("effective model");
        assert_eq!(
            model.value.as_ref().and_then(|m| m.model.as_ref()).unwrap(),
            "new"
        );
        assert_eq!(
            overlay
                .rerouted_from
                .get("t2")
                .and_then(|v| v.value.as_ref())
                .unwrap(),
            "old"
        );
        assert_eq!(
            overlay
                .reroute_reason
                .get("t2")
                .and_then(|v| v.value.as_ref())
                .unwrap(),
            "policy"
        );
    }

    #[test]
    fn runtime_overlay_ignores_unknown_notification_types() {
        let mut overlay = RuntimeOverlay::default();
        overlay
            .apply_line(r#"{"method":"notifications/cancelled","threadId":"t1","reason":"noop"}"#);
        assert!(overlay.effective_model.is_empty());
        assert!(overlay.rerouted_from.is_empty());
        assert!(overlay.reroute_reason.is_empty());
        assert!(overlay
            .warnings
            .iter()
            .any(|w| w.contains("unsupported runtime notification")));
    }
}
