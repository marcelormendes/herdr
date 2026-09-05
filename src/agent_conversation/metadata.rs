//! Bounded, allowlisted native session facts. These are projected to existing
//! pane tokens; transcript paths and provider payloads never cross the API.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

#[derive(Debug, Clone)]
pub(super) struct MetadataRecord {
    entry_id: Option<String>,
    values: HashMap<String, Option<String>>,
}

#[derive(Default)]
pub(super) struct MetadataState {
    records: BTreeMap<u64, MetadataRecord>,
    cached: HashMap<String, Option<String>>,
    dirty: bool,
    #[cfg(test)]
    fold_count: usize,
}

impl MetadataState {
    pub(super) fn ingest(&mut self, records: Vec<(u64, MetadataRecord)>, limit: usize) {
        self.records.extend(records);
        self.dirty = true;
        while self.records.len() > limit {
            self.records.pop_first();
        }
    }

    pub(super) fn values(
        &mut self,
        active_ids: Option<&HashSet<String>>,
    ) -> HashMap<String, Option<String>> {
        if !self.dirty {
            return self.cached.clone();
        }
        #[cfg(test)]
        {
            self.fold_count += 1;
        }
        let mut tokens = HashMap::new();
        for record in self.records.values() {
            if record
                .entry_id
                .as_ref()
                .is_some_and(|id| active_ids.is_some_and(|active| !active.contains(id)))
            {
                continue;
            }
            for (key, value) in &record.values {
                tokens.insert(key.clone(), value.clone());
            }
        }
        self.cached = tokens.clone();
        self.dirty = false;
        tokens
    }

    #[cfg(test)]
    fn tokens(&mut self, active_ids: Option<&HashSet<String>>) -> HashMap<String, String> {
        self.values(active_ids)
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect()
    }
}

fn text(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    (!text.is_empty() && text.len() <= 1024 && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

fn number(value: &Value) -> Option<String> {
    let number = value.as_f64()?;
    (number.is_finite() && number >= 0.0).then(|| value.to_string())
}

fn put_text(tokens: &mut HashMap<String, Option<String>>, key: &str, value: &Value) {
    if let Some(value) = text(value) {
        tokens.insert(key.to_owned(), Some(value));
    }
}

fn put_number(tokens: &mut HashMap<String, Option<String>>, key: &str, value: &Value) {
    if let Some(value) = number(value) {
        tokens.insert(key.to_owned(), Some(value));
    }
}

// A response is a measurement boundary. Clear missing metrics instead of
// combining one response's output/cost with another response's input usage.
fn response_usage(tokens: &mut HashMap<String, Option<String>>, usage: &Value, pi: bool) {
    if !usage.is_object() {
        return;
    }
    for (key, native_key) in if pi {
        [
            ("input_tokens", "input"),
            ("output_tokens", "output"),
            ("cache_read_tokens", "cacheRead"),
            ("cache_write_tokens", "cacheWrite"),
        ]
    } else {
        [
            ("input_tokens", "input_tokens"),
            ("output_tokens", "output_tokens"),
            ("cache_read_tokens", "cache_read_input_tokens"),
            ("cache_write_tokens", "cache_creation_input_tokens"),
        ]
    } {
        tokens.insert(key.into(), number(&usage[native_key]));
    }
    tokens.insert(
        "cost".into(),
        if pi {
            number(&usage["cost"]["total"])
        } else {
            None
        },
    );
    tokens.insert("usage_scope".into(), Some("last_response".into()));
}

pub(super) fn normalize(provider: &str, line: &str) -> Option<MetadataRecord> {
    if !matches!(provider, "pi" | "codex" | "claude") {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let mut tokens = HashMap::new();
    let kind = value["type"].as_str()?;
    let mut entry_id = None;
    match provider {
        "pi" => {
            if kind != "session" {
                entry_id = value["id"].as_str().map(str::to_owned);
            }
            match kind {
                "session" => put_text(&mut tokens, "cwd", &value["cwd"]),
                "model_change" => put_text(&mut tokens, "model", &value["modelId"]),
                "thinking_level_change" => {
                    put_text(&mut tokens, "thinking", &value["thinkingLevel"])
                }
                "message" if value["message"]["role"] == "assistant" => {
                    let message = &value["message"];
                    put_text(&mut tokens, "model", &message["model"]);
                    response_usage(&mut tokens, &message["usage"], true);
                }
                _ => {}
            }
        }
        "codex" => {
            let payload = &value["payload"];
            match kind {
                "session_meta" | "turn_context" => {
                    put_text(&mut tokens, "cwd", &payload["cwd"]);
                    put_text(&mut tokens, "model", &payload["model"]);
                    if kind == "turn_context" {
                        tokens.insert("thinking".into(), text(&payload["effort"]));
                    }
                }
                "event_msg" if payload["type"] == "token_count" => {
                    let info = &payload["info"];
                    let usage = &info["total_token_usage"];
                    if usage.is_object() {
                        for key in ["input_tokens", "output_tokens"] {
                            tokens.insert(key.into(), number(&usage[key]));
                        }
                        tokens.insert(
                            "cache_read_tokens".into(),
                            number(&usage["cached_input_tokens"]),
                        );
                        tokens.insert(
                            "cache_write_tokens".into(),
                            number(&usage["cache_write_input_tokens"]),
                        );
                        tokens.insert("cost".into(), None);
                        tokens.insert("usage_scope".into(), Some("session".into()));
                    }
                    if info.is_object() {
                        tokens.insert("context_percent".into(), None);
                    }
                    put_number(&mut tokens, "context_window", &info["model_context_window"]);
                    put_number(
                        &mut tokens,
                        "context_tokens",
                        &info["last_token_usage"]["total_tokens"],
                    );
                }
                _ => {}
            }
        }
        "claude" => {
            if value["isSidechain"] == true || value["isMeta"] == true {
                return None;
            }
            put_text(&mut tokens, "cwd", &value["cwd"]);
            if kind == "assistant" {
                let message = &value["message"];
                if message["model"] != "<synthetic>" {
                    put_text(&mut tokens, "model", &message["model"]);
                    response_usage(&mut tokens, &message["usage"], false);
                }
            }
        }
        _ => {}
    }
    (!tokens.is_empty()).then_some(MetadataRecord {
        entry_id,
        values: tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(provider: &str, lines: &[&str]) -> HashMap<String, String> {
        let mut state = MetadataState::default();
        state.ingest(
            lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    normalize(provider, line).map(|record| (index as u64, record))
                })
                .collect(),
            100,
        );
        state.tokens(None)
    }

    #[test]
    fn codex_preserves_reported_model_effort_and_session_usage() {
        let actual = tokens(
            "codex",
            &[
                r#"{"type":"turn_context","payload":{"cwd":"/repo","model":"gpt-reported","effort":"high"}}"#,
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2000,"cached_input_tokens":1000,"output_tokens":50},"last_token_usage":{"total_tokens":1050},"model_context_window":100000}}}"#,
            ],
        );
        assert_eq!(actual["model"], "gpt-reported");
        assert_eq!(actual["thinking"], "high");
        assert_eq!(actual["input_tokens"], "2000");
        assert_eq!(actual["cache_read_tokens"], "1000");
        assert_eq!(actual["context_tokens"], "1050");
        assert_eq!(actual["usage_scope"], "session");
        assert!(!actual.contains_key("cost"));
    }

    #[test]
    fn pi_usage_is_last_response_not_invented_session_total() {
        let actual = tokens(
            "pi",
            &[
                r#"{"type":"thinking_level_change","id":"t","thinkingLevel":"off"}"#,
                r#"{"type":"message","id":"a","message":{"role":"assistant","model":"reported","usage":{"input":10,"output":0,"cacheRead":0,"cost":{"total":0}}}}"#,
                r#"{"type":"message","id":"b","message":{"role":"assistant","model":"new","usage":{"input":20}}}"#,
            ],
        );
        assert_eq!(actual["thinking"], "off");
        assert_eq!(actual["model"], "new");
        assert_eq!(actual["input_tokens"], "20");
        assert_eq!(actual["usage_scope"], "last_response");
        assert!(!actual.contains_key("output_tokens"));
        assert!(!actual.contains_key("cost"));
    }

    #[test]
    fn claude_excludes_subagents_and_synthetic_usage() {
        let actual = tokens(
            "claude",
            &[
                r#"{"type":"assistant","cwd":"/repo","message":{"model":"claude-reported","usage":{"input_tokens":0,"output_tokens":12,"cache_creation_input_tokens":50}}}"#,
                r#"{"type":"assistant","isSidechain":true,"message":{"model":"subagent","usage":{"input_tokens":999}}}"#,
                r#"{"type":"assistant","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            ],
        );
        assert_eq!(actual["model"], "claude-reported");
        assert_eq!(actual["input_tokens"], "0");
        assert_eq!(actual["output_tokens"], "12");
        assert_eq!(actual["cache_write_tokens"], "50");
        assert!(!actual.contains_key("thinking"));
    }

    #[test]
    fn history_backfill_does_not_replace_newer_measurements_and_inactive_pi_branch_is_ignored() {
        let mut state = MetadataState::default();
        let line =
            |id, model| format!(r#"{{"type":"model_change","id":"{id}","modelId":"{model}"}}"#);
        state.ingest(
            vec![(100, normalize("pi", &line("active", "new")).unwrap())],
            100,
        );
        state.ingest(
            vec![
                (0, normalize("pi", &line("old", "old")).unwrap()),
                (200, normalize("pi", &line("inactive", "other")).unwrap()),
            ],
            100,
        );
        let active = HashSet::from(["active".into(), "old".into()]);
        assert_eq!(state.tokens(Some(&active))["model"], "new");
    }

    #[test]
    fn unchanged_polls_reuse_bounded_metadata_projection() {
        let mut state = MetadataState::default();
        for index in 0..10 {
            let line =
                format!(r#"{{"type":"turn_context","payload":{{"model":"model-{index}"}}}}"#);
            state.ingest(vec![(index, normalize("codex", &line).unwrap())], 3);
        }
        assert_eq!(state.records.len(), 3);
        assert_eq!(state.tokens(None)["model"], "model-9");
        for _ in 0..20 {
            assert_eq!(state.tokens(None)["model"], "model-9");
        }
        assert_eq!(state.fold_count, 1);
    }

    #[test]
    fn metadata_is_bounded_and_never_exposes_arbitrary_payload_fields() {
        assert!(normalize("omp", r#"{"type":"session","cwd":"/repo"}"#).is_none());
        assert!(tokens("codex", &[r#"{"type":"turn_context","payload":{"model":"\u001b[secret","effort":"","private":"secret"}}"#]).is_empty());
        let actual = tokens(
            "pi",
            &[
                r#"{"type":"message","message":{"role":"assistant","usage":{"input":-1,"output":"unknown","cost":{"total":-1}}}}"#,
            ],
        );
        assert!(!actual.contains_key("input_tokens"));
        assert!(!actual.contains_key("output_tokens"));
        assert!(!actual.contains_key("cost"));
    }
}
