//! Cherry Studio (desktop client) agent-session usage parser.
//!
//! Cherry Studio's Agent / Claude Code sessions write **standard Claude Code
//! transcripts** under its per-user app-data directory:
//! `%APPDATA%\CherryStudio\.claude\projects\<workspace>\<session>.jsonl`
//! (macOS: `~/Library/Application Support/CherryStudio/.claude/projects/...`,
//! Linux: `$XDG_CONFIG_HOME/CherryStudio/.claude/projects/...`).
//!
//! Unlike a stock Claude Code transcript, Cherry Studio appends the **same API
//! call to the file 3-4 times** (different `uuid`, identical `usage`) as the
//! streaming response progresses. Naively summing every assistant row
//! triple-counts each call (verified ~3x over the true figure). The canonical
//! fix — validated against DeepSeek's platform per-hour billing, <1% error —
//! is to dedupe **consecutive rows within a session whose usage signature
//! (model + the four token buckets) is identical**, keeping one record per API
//! call. This parser implements exactly that; all reads are strictly read-only.
//!
//! The usage fields come from the assistant event's `message.usage`:
//! `input_tokens` (cache miss), `cache_read_input_tokens` (cache hit),
//! `cache_creation_input_tokens` (cache write) and `output_tokens`.

use super::utils::{file_modified_timestamp_ms, parse_timestamp_str};
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;

const CLIENT_ID: &str = "cherrystudio";

fn provider_for_model(model: &str) -> &'static str {
    let lower = model.to_lowercase();
    if lower.contains("deepseek") {
        "deepseek"
    } else if lower.contains("claude") {
        "anthropic"
    } else if lower.contains("gpt")
        || lower.contains("o1")
        || lower.contains("o3")
        || lower.contains("o4")
        || lower.contains("sol")
    {
        "openai"
    } else {
        "unknown"
    }
}

/// Derive the workspace key from a transcript path by finding the
/// `.claude/projects/<slug>` window — same logic as the Claude Code parser, and
/// Cherry Studio's layout matches it exactly.
fn workspace_from_path(path: &Path) -> (Option<String>, Option<String>) {
    let components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    for window in components.windows(3) {
        if window[0] == ".claude" && window[1] == "projects" {
            let key = normalize_workspace_key(&window[2]);
            let label = key.as_deref().and_then(workspace_label_from_key);
            return (key, label);
        }
    }
    (None, None)
}

/// Parse a Cherry Studio Claude Code transcript into unified messages, applying
/// the per-session consecutive usage-signature dedup that matches the canonical
/// platform-validated figures.
pub fn parse_cherrystudio_file(path: &Path) -> Vec<UnifiedMessage> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let fallback_timestamp = file_modified_timestamp_ms(path);
    let (workspace_key, workspace_label) = workspace_from_path(path);

    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut last_signature: Option<String> = None;
    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(message) = record.get("message").and_then(Value::as_object) else {
            continue;
        };
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            continue;
        };

        let input = usage
            .get("input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0);
        let output = usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0);
        if input == 0 && output == 0 {
            continue;
        }

        let model = message
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if model.is_empty() || model == "<synthetic>" || model.eq_ignore_ascii_case("unknown") {
            continue;
        }

        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0);
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0);
        let total = input + output + cache_read + cache_creation;
        if total <= 0 {
            continue;
        }

        // Canonical dedup: Cherry Studio writes the same API call 3-4 times with
        // identical usage. Skip a row whose usage signature matches the previous
        // one in this file (one session = one file).
        let signature =
            format!("{model}\u{1f}{input}\u{1f}{cache_read}\u{1f}{cache_creation}\u{1f}{output}");
        if last_signature.as_deref() == Some(&signature) {
            continue;
        }
        last_signature = Some(signature);

        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_str)
            .unwrap_or(fallback_timestamp);

        let tokens = TokenBreakdown {
            input,
            output,
            cache_read,
            cache_write: cache_creation,
            reasoning: 0,
        };

        let provider = provider_for_model(&model);
        let mut msg = UnifiedMessage::new(
            CLIENT_ID,
            model,
            provider,
            session_id.clone(),
            timestamp,
            tokens,
            0.0,
        );
        if let (Some(key), Some(label)) = (workspace_key.clone(), workspace_label.clone()) {
            msg.set_workspace(Some(key), Some(label));
        }
        messages.push(msg);
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_transcript(dir: &std::path::Path, name: &str, lines: &[&str]) -> std::path::PathBuf {
        let path = dir
            .join(".claude")
            .join("projects")
            .join("D--repo")
            .join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn dedupes_consecutive_identical_usage_signatures() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                // The same API call appended 3 times with identical usage.
                r#"{"type":"assistant","sessionId":"s1","uuid":"a","timestamp":"2026-04-27T13:59:02.828Z","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":50,"output_tokens":30}}}"#,
                r#"{"type":"assistant","sessionId":"s1","uuid":"b","timestamp":"2026-04-27T13:59:02.900Z","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":50,"output_tokens":30}}}"#,
                r#"{"type":"assistant","sessionId":"s1","uuid":"c","timestamp":"2026-04-27T13:59:03.000Z","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":50,"output_tokens":30}}}"#,
                // A genuinely different call.
                r#"{"type":"assistant","sessionId":"s1","uuid":"d","timestamp":"2026-04-27T14:00:00.000Z","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":40,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":10}}}"#,
            ],
        );
        let messages = parse_cherrystudio_file(&path);
        assert_eq!(
            messages.len(),
            2,
            "three identical signatures collapse to one"
        );
        assert_eq!(messages[0].tokens.total(), 380);
        assert_eq!(messages[1].tokens.total(), 50);
        assert_eq!(messages[0].workspace_key.as_deref(), Some("D--repo"));
    }

    #[test]
    #[ignore]
    fn real_transcripts_dedup_count() {
        let appdata = std::env::var("APPDATA").expect("APPDATA set");
        let base = std::path::Path::new(&appdata)
            .join("CherryStudio")
            .join(".claude")
            .join("projects");
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if dir.is_dir() {
                    if let Ok(items) = std::fs::read_dir(&dir) {
                        for item in items.flatten() {
                            if item.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                                files.push(item.path());
                            }
                        }
                    }
                }
            }
        }
        files.sort();
        let mut total_messages = 0usize;
        let mut total_tokens = 0i64;
        let mut by_model: std::collections::HashMap<String, (usize, i64)> = Default::default();
        for path in &files {
            for msg in parse_cherrystudio_file(path) {
                total_messages += 1;
                total_tokens += msg.tokens.total();
                let e = by_model.entry(msg.model_id.clone()).or_default();
                e.0 += 1;
                e.1 += msg.tokens.total();
            }
        }
        println!("真实转录文件数: {}", files.len());
        println!("去重后总消息数: {}", total_messages);
        println!("去重后总 token: {}", total_tokens);
        let mut models: Vec<_> = by_model.into_iter().collect();
        models.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
        for (m, (c, t)) in models {
            println!("  {m:<24} msgs={c:>6}  tokens={t:>14}");
        }
        assert!(total_messages > 0);
    }

    #[test]
    fn keeps_non_consecutive_same_usage_as_separate_calls() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                r#"{"type":"assistant","sessionId":"s1","uuid":"a","timestamp":"2026-04-27T13:59:02.828Z","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","sessionId":"s1","uuid":"b","timestamp":"2026-04-27T13:59:05.000Z","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":200,"output_tokens":20}}}"#,
                r#"{"type":"assistant","sessionId":"s1","uuid":"c","timestamp":"2026-04-27T14:00:00.000Z","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );
        let messages = parse_cherrystudio_file(&path);
        // The third row has the same signature as the first, but is not
        // consecutive, so it is a distinct call and must be kept.
        assert_eq!(messages.len(), 3);
    }
}
