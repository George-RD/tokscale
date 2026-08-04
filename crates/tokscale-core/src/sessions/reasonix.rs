//! Reasonix session parser
//!
//! Parses transcript JSONL files from `~/.reasonix/projects/**/sessions/*.jsonl`.
//!
//! Reasonix does not currently expose authoritative token usage in the
//! transcript or sidecar metadata we found locally, so we estimate usage from
//! the text that appears in each turn. Assistant entries are treated as the
//! output for a turn; preceding system/user/tool entries are accumulated as the
//! input for that turn. This keeps Reasonix usable in Tokscale without making
//! up unsupported per-message counters.

use super::utils::{file_modified_timestamp_ms, parse_timestamp_str};
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::provider_identity::inferred_provider_from_model;
use crate::TokenBreakdown;
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
struct ReasonixSessionMeta {
    id: Option<String>,
    model: Option<String>,
    #[allow(dead_code)]
    turns: Option<i64>,
    #[allow(dead_code)]
    preview: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReasonixTurnFile {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReasonixTurn {
    #[allow(dead_code)]
    turn: Option<i64>,
    time: Option<String>,
    #[allow(dead_code)]
    prompt: Option<String>,
    #[serde(rename = "msgIndex")]
    msg_index: Option<i64>,
    files: Option<Vec<ReasonixTurnFile>>,
}

fn load_reasonix_meta(path: &Path) -> ReasonixSessionMeta {
    let meta_path = path.with_extension("jsonl.meta");
    let Ok(content) = std::fs::read_to_string(meta_path) else {
        return ReasonixSessionMeta::default();
    };

    serde_json::from_str(&content).unwrap_or_default()
}

pub(crate) fn reasonix_related_paths(path: &Path) -> Vec<(String, PathBuf)> {
    let mut related = vec![(".meta".to_string(), path.with_extension("jsonl.meta"))];
    let Some(session_dir) = path.parent() else {
        return related;
    };
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return related;
    };
    let checkpoint_dir = session_dir.join(format!("{stem}.ckpt"));
    let Ok(entries) = std::fs::read_dir(&checkpoint_dir) else {
        return related;
    };
    let mut checkpoints: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|entry| {
            entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("turn-") && name.ends_with(".json"))
        })
        .collect();
    checkpoints.sort_unstable();
    related.extend(checkpoints.into_iter().map(|checkpoint| {
        let suffix = checkpoint
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!(".ckpt/{name}"))
            .unwrap_or_else(|| ".ckpt/unknown".to_string());
        (suffix, checkpoint)
    }));
    related
}

fn reasonix_turns(path: &Path) -> Vec<ReasonixTurn> {
    reasonix_related_paths(path)
        .into_iter()
        .filter(|(suffix, _)| suffix.starts_with(".ckpt/"))
        .filter_map(|(_, turn_path)| std::fs::read_to_string(turn_path).ok())
        .filter_map(|content| serde_json::from_str::<ReasonixTurn>(&content).ok())
        .collect()
}

fn reasonix_common_workspace_root(turns: &[ReasonixTurn]) -> Option<String> {
    let mut candidate_paths: Vec<PathBuf> = Vec::new();
    for turn in turns {
        let Some(files) = turn.files.as_ref() else {
            continue;
        };

        for file in files {
            let Some(path) = file.path.as_deref() else {
                continue;
            };
            let Some(normalized) = normalize_workspace_key(path) else {
                continue;
            };
            let normalized_path = PathBuf::from(normalized);
            candidate_paths.push(
                normalized_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(normalized_path),
            );
        }
        if !candidate_paths.is_empty() {
            break;
        }
    }

    common_path_prefix(candidate_paths.iter().map(PathBuf::as_path))
        .and_then(|root| normalize_workspace_key(&root.to_string_lossy()))
}

fn common_path_prefix<'a>(mut paths: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    let first = paths.next()?;
    prefix.push(first);

    for path in paths {
        prefix = shared_prefix(&prefix, path)?;
    }

    if prefix.as_os_str().is_empty() || prefix.components().count() <= 1 {
        None
    } else {
        Some(prefix)
    }
}

fn shared_prefix(left: &Path, right: &Path) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    let mut left_components = left.components();
    let mut right_components = right.components();

    loop {
        match (left_components.next(), right_components.next()) {
            (Some(a), Some(b)) if a == b => result.push(a.as_os_str()),
            _ => break,
        }
    }

    if result.as_os_str().is_empty() {
        None
    } else {
        Some(result)
    }
}

fn json_text_len(value: &Value) -> usize {
    match value {
        Value::String(text) => text.chars().count(),
        Value::Array(items) => items.iter().map(json_text_len).sum(),
        Value::Object(map) => map.values().map(json_text_len).sum(),
        _ => 0,
    }
}

fn transcript_input_chars(value: &Value) -> usize {
    value.get("content").map(json_text_len).unwrap_or_default()
}

fn transcript_output_chars(value: &Value) -> usize {
    value.get("content").map(json_text_len).unwrap_or_default()
        + value
            .get("reasoning_content")
            .map(json_text_len)
            .unwrap_or_default()
        + value
            .get("tool_calls")
            .map(json_text_len)
            .unwrap_or_default()
}

fn estimate_tokens(chars: usize) -> i64 {
    chars.div_ceil(4) as i64
}

fn timestamp_for_assistant(
    assistant_index: usize,
    checkpoint_turns: &[ReasonixTurn],
    fallback_timestamp: i64,
) -> i64 {
    // Reasonix checkpoints store the transcript position (`msgIndex`) and a
    // wall-clock turn timestamp. Pair the checkpoint with the assistant at
    // that index when available; otherwise pair in checkpoint order. The
    // ordinal fallback is explicit and local to sources without a usable
    // `msgIndex`, rather than silently putting every historic response at the
    // transcript file's mtime.
    let transcript_position = assistant_index.saturating_mul(2).saturating_add(1) as i64;
    let checkpoint = checkpoint_turns
        .iter()
        .find(|turn| turn.msg_index == Some(transcript_position))
        .or_else(|| checkpoint_turns.get(assistant_index));
    checkpoint
        .and_then(|turn| turn.time.as_deref())
        .and_then(parse_timestamp_str)
        .unwrap_or(fallback_timestamp)
}

pub fn parse_reasonix_file(path: &Path) -> Vec<UnifiedMessage> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };

    let fallback_timestamp = file_modified_timestamp_ms(path);
    let meta = load_reasonix_meta(path);
    let session_id = meta.id.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string()
    });
    let model_id = meta.model.unwrap_or_else(|| "unknown".to_string());
    let checkpoint_turns = reasonix_turns(path);
    let workspace_key = reasonix_common_workspace_root(&checkpoint_turns);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);

    let reader = BufReader::new(file);
    let mut messages: Vec<UnifiedMessage> = Vec::with_capacity(32);
    let mut buffer = Vec::with_capacity(4096);
    let mut pending_input_chars: usize = 0;
    let mut turn_has_input = false;
    let mut assistant_index: usize = 0;

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        buffer.clear();
        buffer.extend_from_slice(trimmed.as_bytes());
        let entry: Value = match simd_json::from_slice(&mut buffer) {
            Ok(value) => value,
            Err(_) => continue,
        };

        match entry.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                let output_chars = transcript_output_chars(&entry);
                if output_chars == 0 && pending_input_chars == 0 {
                    continue;
                }

                let tokens = TokenBreakdown {
                    input: estimate_tokens(pending_input_chars),
                    output: estimate_tokens(output_chars),
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                };

                if tokens.total() <= 0 {
                    pending_input_chars = 0;
                    turn_has_input = false;
                    continue;
                }

                let mut unified = UnifiedMessage::new_with_dedup(
                    "reasonix",
                    model_id.clone(),
                    inferred_provider_from_model(&model_id).unwrap_or("reasonix"),
                    session_id.clone(),
                    timestamp_for_assistant(assistant_index, &checkpoint_turns, fallback_timestamp),
                    tokens,
                    0.0,
                    Some(format!("reasonix:{session_id}:{assistant_index}")),
                );
                unified.is_turn_start = turn_has_input;
                unified.set_workspace(workspace_key.clone(), workspace_label.clone());
                messages.push(unified);

                assistant_index += 1;
                pending_input_chars = 0;
                turn_has_input = false;
            }
            Some(_) => {
                let input_chars = transcript_input_chars(&entry);
                if input_chars > 0 {
                    pending_input_chars += input_chars;
                    if !matches!(entry.get("role").and_then(Value::as_str), Some("tool")) {
                        turn_has_input = true;
                    }
                }
            }
            None => continue,
        }
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_file(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn parse_reasonix_file_estimates_usage_and_reads_workspace_from_turn_files() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let file_path = workspace.join("README.md");
        write_file(&file_path, "# demo");

        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let transcript_path =
            sessions_dir.join("20260620-123527.683793941-deepseek-v4-flash.jsonl");
        write_file(
            &transcript_path,
            r#"{"role":"system","content":"System prompt"}
{"role":"user","content":"Please inspect README.md"}
{"role":"assistant","reasoning_content":"I will check the file.","tool_calls":[{"id":"call_1","name":"read_file","arguments":"{\"path\":\"README.md\"}"}]}
{"role":"tool","content":"file contents"}
{"role":"assistant","content":"Done"}
"#,
        );
        write_file(
            &transcript_path.with_extension("jsonl.meta"),
            r#"{"id":"reasonix-session-001","model":"opencode/deepseek-v4-flash","turns":1,"preview":"Please inspect README.md"}"#,
        );

        let ckpt_dir = sessions_dir.join("20260620-123527.683793941-deepseek-v4-flash.ckpt");
        std::fs::create_dir_all(&ckpt_dir).unwrap();
        write_file(
            &ckpt_dir.join("turn-0.json"),
            &format!(
                r#"{{"turn":0,"time":"2026-06-20T18:05:47.687906482+05:30","prompt":"Please inspect README.md","msgIndex":1,"files":[{{"path":"{}","content":null}}]}}"#,
                file_path.display()
            ),
        );

        let messages = parse_reasonix_file(&transcript_path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].client, "reasonix");
        assert_eq!(messages[0].session_id, "reasonix-session-001");
        assert_eq!(messages[0].model_id, "opencode/deepseek-v4-flash");
        assert_eq!(messages[0].provider_id, "deepseek");
        assert!(messages[0].tokens.input > 0);
        assert!(messages[0].tokens.output > 0);
        assert_eq!(messages[0].workspace_label.as_deref(), Some("workspace"));
        assert!(messages[0].is_turn_start);
        assert!(!messages[1].is_turn_start);
        assert_eq!(
            messages[1].tokens.output,
            estimate_tokens("Done".chars().count())
        );
        assert_eq!(
            messages[0].timestamp,
            parse_timestamp_str("2026-06-20T18:05:47.687906482+05:30").unwrap()
        );
    }

    #[test]
    fn checkpoint_timestamp_uses_ordinal_approximation_when_msg_index_is_missing() {
        let root = tempdir().unwrap();
        let transcript = root.path().join("session.jsonl");
        write_file(
            &transcript,
            r#"{"role":"user","content":"one"}
{"role":"assistant","content":"one"}
{"role":"user","content":"two"}
{"role":"assistant","content":"two"}
"#,
        );
        let checkpoint_dir = root.path().join("session.ckpt");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        write_file(
            &checkpoint_dir.join("turn-0.json"),
            r#"{"time":"2026-06-20T10:00:00Z"}"#,
        );
        write_file(
            &checkpoint_dir.join("turn-1.json"),
            r#"{"time":"2026-06-21T11:00:00Z"}"#,
        );

        let messages = parse_reasonix_file(&transcript);
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].timestamp,
            parse_timestamp_str("2026-06-20T10:00:00Z").unwrap()
        );
        assert_eq!(
            messages[1].timestamp,
            parse_timestamp_str("2026-06-21T11:00:00Z").unwrap()
        );
    }
}
