//! Parser for Reasonix's authoritative append-only statistics records.
//!
//! Reasonix writes one JSON object per provider request to
//! `<REASONIX_HOME>/stats/YYYY-MM-DD.jsonl`. Session transcript JSONL is not
//! scanned: it has no authoritative usage counters and would overlap stats.

use super::utils::parse_timestamp_value;
use super::UnifiedMessage;
use crate::provider_identity::{canonical_provider, inferred_provider_from_model};
use crate::TokenBreakdown;
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Deserialize)]
struct ReasonixStat {
    ts: serde_json::Value,
    #[serde(default)]
    model: String,
    #[serde(default)]
    prompt: i64,
    #[serde(default)]
    completion: i64,
    #[serde(default)]
    reasoning: i64,
    #[serde(default)]
    cache_hit: i64,
    #[serde(default)]
    cache_miss: i64,
    #[serde(default)]
    total: i64,
    #[serde(default)]
    requests: i64,
    #[serde(default)]
    turn: bool,
}

fn split_model_ref(model_ref: &str) -> (String, String) {
    let model_ref = model_ref.trim();
    if let Some((provider, model)) = model_ref.split_once('/') {
        // Reasonix may label an upstream model with its OpenCode-compatible
        // routing surface. Preserve the real provider for pricing/grouping.
        if matches!(provider, "opencode" | "openrouter" | "router") {
            if let Some(inferred) = inferred_provider_from_model(model) {
                return (inferred.to_string(), model.to_string());
            }
        }
        if let Some(provider) = canonical_provider(provider) {
            return (provider, model.to_string());
        }
    }
    let provider = inferred_provider_from_model(model_ref)
        .unwrap_or("reasonix")
        .to_string();
    (provider, model_ref.to_string())
}

fn non_negative(value: i64) -> i64 {
    value.max(0)
}

pub fn parse_reasonix_file(path: &Path) -> Vec<UnifiedMessage> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(line_index, line)| {
            let line = line.ok()?;
            let record: ReasonixStat = serde_json::from_str(line.trim()).ok()?;
            if record.turn || record.model.trim().is_empty() || record.total <= 0 {
                return None;
            }
            let timestamp = parse_timestamp_value(&record.ts)?;
            let (provider_id, model_id) = split_model_ref(&record.model);
            let cache_read = non_negative(record.cache_hit);
            let cache_miss = non_negative(record.cache_miss);
            let raw_input = non_negative(record.prompt);
            // Reasonix defines cache_miss as prompt tokens not served from a
            // cache. It is ordinary input, not a cache-creation/write charge.
            let input = if cache_miss > 0 {
                cache_miss
            } else {
                raw_input.saturating_sub(cache_read)
            };
            let reasoning = non_negative(record.reasoning).min(non_negative(record.completion));
            let tokens = TokenBreakdown {
                input,
                output: non_negative(record.completion).saturating_sub(reasoning),
                cache_read,
                cache_write: 0,
                reasoning,
            };
            if tokens.total() <= 0 {
                return None;
            }

            Some(UnifiedMessage::new_with_dedup(
                "reasonix",
                model_id,
                provider_id,
                format!("reasonix-stats:{}", path.display()),
                timestamp,
                tokens,
                0.0,
                Some(format!(
                    "reasonix:{}:{}:{}:{}",
                    path.display(),
                    line_index,
                    record.requests,
                    record.total
                )),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn parses_authoritative_stats_with_provider_usage_and_timestamp() {
        let file = NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                "{\"ts\":\"2026-08-04T09:10:11Z\",\"model\":\"opencode/deepseek-v4\",\"prompt\":100,\"completion\":20,\"reasoning\":5,\"cache_hit\":30,\"cache_miss\":10,\"total\":120,\"requests\":1}\n",
                "{\"ts\":\"2026-08-04T09:11:11Z\",\"turn\":true}\n",
            ),
        )
        .unwrap();

        let messages = parse_reasonix_file(file.path());
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.client, "reasonix");
        assert_eq!(message.provider_id, "deepseek");
        assert_eq!(message.model_id, "deepseek-v4");
        assert_eq!(message.tokens.input, 10);
        assert_eq!(message.tokens.output, 15);
        assert_eq!(message.tokens.reasoning, 5);
        assert_eq!(message.tokens.cache_read, 30);
        assert_eq!(message.tokens.cache_write, 0);
        assert_eq!(
            message.timestamp,
            parse_timestamp_value(&serde_json::json!("2026-08-04T09:10:11Z")).unwrap()
        );
    }

    #[test]
    fn skips_turn_markers_malformed_and_zero_usage_records() {
        let file = NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                "not json\n",
                "{\"ts\":\"2026-08-04T09:10:11Z\",\"turn\":true}\n",
                "{\"ts\":\"2026-08-04T09:10:11Z\",\"model\":\"deepseek/test\",\"total\":0}\n",
            ),
        )
        .unwrap();
        assert!(parse_reasonix_file(file.path()).is_empty());
    }

    #[test]
    fn preserves_unknown_model_provider_as_reasonix_only_when_not_inferable() {
        assert_eq!(
            split_model_ref("deepseek/chat"),
            ("deepseek".into(), "chat".into())
        );
        assert_eq!(
            split_model_ref("claude-sonnet-4"),
            ("anthropic".into(), "claude-sonnet-4".into())
        );
    }
}
