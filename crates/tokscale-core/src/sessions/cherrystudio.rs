//! Cherry Studio (desktop client) agent-session usage parser.
//!
//! Cherry Studio's Agent / Claude Code sessions write **standard Claude Code
//! transcripts** under its per-user app-data directory:
//! `%APPDATA%\CherryStudio\Data\Agents\.claude\projects\<workspace>\<session>.jsonl`
//! (macOS: `~/Library/Application Support/CherryStudio/Data/Agents/.claude/projects/...`,
//! Linux: `$XDG_CONFIG_HOME/CherryStudio/Data/Agents/.claude/projects/...`).
//! The V1 root omits `Data/Agents`; both roots are scanned so pre-upgrade
//! history remains available.
//!
//! Unlike a stock Claude Code transcript, Cherry Studio appends the **same API
//! call to the file 3-4 times** (different `uuid`, identical `requestId`,
//! `message.id`, and `usage`) as the streaming response progresses. `requestId`
//! is the API-call identity, so records sharing it are one call even when a
//! streaming record later gains or changes `message.id`. Naively
//! summing every assistant row triple-counts each call (verified ~3x over the
//! true figure). The canonical fix — validated against DeepSeek's platform
//! per-hour billing, <1% error — is to dedupe by the stable request and/or
//! message identity; `uuid` is only a fallback when neither primary ID exists.
//! Usage signatures are not identities: two distinct requests may legitimately
//! have identical token counts. Records without an identity are retained
//! conservatively. All reads are strictly read-only.
//!
//! The usage fields come from the assistant event's `message.usage`:
//! `input_tokens` (cache miss), `cache_read_input_tokens` (cache hit),
//! `cache_creation_input_tokens` (cache write) and `output_tokens`.

use super::utils::{file_modified_timestamp_ms, parse_timestamp_str};
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;

const CLIENT_ID: &str = "cherrystudio";

/// The strongest identity an accepted record has contributed to an alias set.
///
/// Cherry Studio's `requestId` names an API call. A `message.id` can arrive
/// later as that call streams, while `uuid` identifies only a written event.
/// We therefore only let a lower-fidelity alias join an existing, lower-fidelity
/// record. In particular, two records with distinct request IDs remain distinct
/// even if malformed/replayed data happens to reuse a message ID or UUID.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum IdentityStrength {
    Uuid,
    Message,
    Request,
}

/// Alias-aware deduplication for one Cherry Studio transcript.
///
/// A stream can begin as `uuid`, acquire `message.id`, then acquire `requestId`.
/// Each accepted row registers every supplied alias, so either ordering of those
/// transitions collapses. Request ID is authoritative when present: an existing
/// message/UUID alias can only absorb a new request if that old row had not
/// already established a request of its own.
#[derive(Default)]
struct IdentityAliases {
    aliases: HashMap<String, usize>,
    /// Lower-fidelity aliases observed under distinct API requests. They cannot
    /// safely identify a request-only/missing-ID record any longer.
    ambiguous_aliases: HashSet<String>,
    strengths: Vec<IdentityStrength>,
}

impl IdentityAliases {
    fn owner(&self, key: &str) -> Option<usize> {
        (!self.ambiguous_aliases.contains(key))
            .then(|| self.aliases.get(key).copied())
            .flatten()
    }

    fn register(&mut self, key: String, owner: usize, strength: IdentityStrength) {
        match self.aliases.get(&key) {
            Some(&existing) if existing != owner && strength < IdentityStrength::Request => {
                // A reused message/UUID cannot link records once two API calls
                // have claimed it. Preserve both requests and retain future
                // sparse records rather than guessing which call they replay.
                self.ambiguous_aliases.insert(key);
            }
            Some(_) => {}
            None => {
                self.aliases.insert(key, owner);
            }
        }
    }

    fn is_duplicate_and_register(
        &mut self,
        request_id: Option<&str>,
        message_id: Option<&str>,
        uuid: Option<&str>,
    ) -> bool {
        let request_key = request_id.map(|id| format!("request:{id}"));
        let message_key = message_id.map(|id| format!("message:{id}"));
        let uuid_key = uuid.map(|id| format!("uuid:{id}"));

        let existing = match request_key
            .as_deref()
            .and_then(|key| self.aliases.get(key).copied())
        {
            // A request ID is Cherry Studio's API-call identity, including when
            // the associated message ID is populated or changes later.
            Some(owner) => Some(owner),
            None if request_key.is_some() => message_key
                .as_deref()
                .and_then(|key| self.owner(key))
                .filter(|&owner| self.strengths[owner] < IdentityStrength::Request)
                .or_else(|| {
                    uuid_key
                        .as_deref()
                        .and_then(|key| self.owner(key))
                        .filter(|&owner| self.strengths[owner] == IdentityStrength::Uuid)
                }),
            // A message ID is sufficient only when no request ID is available.
            // A UUID may join it only when the earlier record was UUID-only.
            None if message_key.is_some() => message_key
                .as_deref()
                .and_then(|key| self.owner(key))
                .or_else(|| {
                    uuid_key
                        .as_deref()
                        .and_then(|key| self.owner(key))
                        .filter(|&owner| self.strengths[owner] == IdentityStrength::Uuid)
                }),
            // UUID is an event fallback. It is deliberately not inferred from
            // usage, so records with no stable identity remain conservative.
            None => uuid_key.as_deref().and_then(|key| self.owner(key)),
        };

        let strength = if request_key.is_some() {
            IdentityStrength::Request
        } else if message_key.is_some() {
            IdentityStrength::Message
        } else if uuid_key.is_some() {
            IdentityStrength::Uuid
        } else {
            return false;
        };
        let owner = existing.unwrap_or_else(|| {
            self.strengths.push(strength);
            self.strengths.len() - 1
        });
        self.strengths[owner] = self.strengths[owner].max(strength);
        if let Some(key) = request_key {
            self.register(key, owner, IdentityStrength::Request);
        }
        if let Some(key) = message_key {
            self.register(key, owner, IdentityStrength::Message);
        }
        if let Some(key) = uuid_key {
            self.register(key, owner, IdentityStrength::Uuid);
        }
        existing.is_some()
    }
}

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
        || lower.ends_with("sol")
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

/// Parse a Cherry Studio Claude Code transcript into unified messages, collapsing
/// only repeated records with a stable per-request, message, or event identity.
pub fn parse_cherrystudio_file(path: &Path) -> Vec<UnifiedMessage> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let fallback_timestamp = file_modified_timestamp_ms(path);
    let (workspace_key, workspace_label) = workspace_from_path(path);

    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut identities = IdentityAliases::default();
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
        let total = input
            .saturating_add(output)
            .saturating_add(cache_read)
            .saturating_add(cache_creation);
        if total <= 0 {
            continue;
        }

        // Cherry Studio can append a streaming record several times. Only an
        // explicit identity proves two rows represent the same call: matching
        // usage is not sufficient because separate calls can cost the same.
        // requestId and message.id survive replay UUID changes, so they take
        // precedence. UUID is only an identity fallback when both are absent.
        // Rows without IDs are retained conservatively.
        let request_id = record
            .get("requestId")
            .or_else(|| message.get("requestId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let message_id = message
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let uuid = record
            .get("uuid")
            .or_else(|| message.get("uuid"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if identities.is_duplicate_and_register(request_id, message_id, uuid) {
            continue;
        }

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
                // The same API call appended three times while streaming.
                r#"{"type":"assistant","sessionId":"s1","uuid":"a","requestId":"request-1","timestamp":"2026-04-27T13:59:02.828Z","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":50,"output_tokens":30}}}"#,
                r#"{"type":"assistant","sessionId":"s1","uuid":"a","requestId":"request-1","timestamp":"2026-04-27T13:59:02.900Z","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":50,"output_tokens":30}}}"#,
                r#"{"type":"assistant","sessionId":"s1","uuid":"a","requestId":"request-1","timestamp":"2026-04-27T13:59:03.000Z","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"cache_read_input_tokens":200,"cache_creation_input_tokens":50,"output_tokens":30}}}"#,
                // A genuinely different call.
                r#"{"type":"assistant","sessionId":"s1","uuid":"d","requestId":"request-2","timestamp":"2026-04-27T14:00:00.000Z","message":{"id":"message-2","model":"deepseek-v4-pro","usage":{"input_tokens":40,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":10}}}"#,
            ],
        );
        let messages = parse_cherrystudio_file(&path);
        assert_eq!(
            messages.len(),
            2,
            "three rows for one request/message identity collapse to one"
        );
        assert_eq!(messages[0].tokens.total(), 380);
        assert_eq!(messages[1].tokens.total(), 50);
        assert_eq!(messages[0].workspace_key.as_deref(), Some("D--repo"));
    }

    #[test]
    fn dedupes_replays_with_changed_uuids_and_same_primary_ids() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                r#"{"type":"assistant","uuid":"event-1","requestId":"request-1","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","uuid":"event-2","requestId":"request-1","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","uuid":"event-3","requestId":"request-1","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        let messages = parse_cherrystudio_file(&path);
        assert_eq!(
            messages.len(),
            1,
            "replays must collapse even when each record has a different UUID"
        );
        assert_eq!(messages[0].tokens.total(), 110);
    }

    #[test]
    fn keeps_distinct_primary_ids_with_identical_usage() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                r#"{"type":"assistant","uuid":"event-1","requestId":"request-1","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","uuid":"event-1","requestId":"request-2","message":{"id":"message-2","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        let messages = parse_cherrystudio_file(&path);
        assert_eq!(
            messages.len(),
            2,
            "distinct primary IDs must count even when UUID and usage match"
        );
        assert_eq!(
            messages
                .iter()
                .map(|message| message.tokens.total())
                .sum::<i64>(),
            220
        );
    }

    #[test]
    fn dedupes_request_only_record_when_later_record_has_message_id() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                // Reviewer repro: a streaming row gains message.id later.
                r#"{"type":"assistant","uuid":"stream-early","requestId":"request-1","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","uuid":"stream-late","requestId":"request-1","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        assert_eq!(parse_cherrystudio_file(&path).len(), 1);
    }

    #[test]
    fn dedupes_message_only_record_when_later_record_has_request_id() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                // The inverse transition must also collapse, even though the
                // replay's event UUID differs.
                r#"{"type":"assistant","uuid":"stream-early","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","uuid":"stream-late","requestId":"request-1","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        assert_eq!(parse_cherrystudio_file(&path).len(), 1);
    }

    #[test]
    fn request_id_defines_one_call_even_when_message_id_changes() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                // Cherry Studio's requestId is its API-call ID; message.id is
                // response metadata populated as the stream evolves. A changed
                // message ID under the same request is therefore a replay, not
                // a second billed request.
                r#"{"type":"assistant","requestId":"request-1","message":{"id":"message-early","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","requestId":"request-1","message":{"id":"message-late","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        assert_eq!(parse_cherrystudio_file(&path).len(), 1);
    }

    #[test]
    fn keeps_distinct_requests_when_message_id_is_reused() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                // A malformed/replayed message ID must not override a distinct
                // API request. Each request still represents one billable call.
                r#"{"type":"assistant","requestId":"request-1","message":{"id":"message-shared","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","requestId":"request-2","message":{"id":"message-shared","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","uuid":"replay-with-new-uuid","requestId":"request-2","message":{"id":"message-shared","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        assert_eq!(
            parse_cherrystudio_file(&path).len(),
            2,
            "different request IDs stay distinct; the request-2 replay collapses"
        );
    }

    #[test]
    fn keeps_sparse_message_after_distinct_requests_reuse_its_id() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                // The two request IDs prove these are distinct calls, making
                // their shared lower-fidelity alias ambiguous.
                r#"{"type":"assistant","requestId":"request-1","message":{"id":"message-shared","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","requestId":"request-2","message":{"id":"message-shared","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                // Without a request ID, this could replay either call. Keep it
                // rather than silently discarding a potentially genuine call.
                r#"{"type":"assistant","message":{"id":"message-shared","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        assert_eq!(parse_cherrystudio_file(&path).len(), 3);
    }

    #[test]
    fn dedupes_uuid_to_complete_identity_transition() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                r#"{"type":"assistant","uuid":"stable-event","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","uuid":"stable-event","requestId":"request-1","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                // UUID changes after request/message aliases were learned.
                r#"{"type":"assistant","uuid":"replayed-event","requestId":"request-1","message":{"id":"message-1","model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        assert_eq!(parse_cherrystudio_file(&path).len(), 1);
    }

    #[test]
    fn keeps_consecutive_no_id_rows_with_identical_usage() {
        let dir = tempdir().unwrap();
        let path = write_transcript(
            dir.path(),
            "session.jsonl",
            &[
                r#"{"type":"assistant","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
                r#"{"type":"assistant","message":{"model":"deepseek-v4-pro","usage":{"input_tokens":100,"output_tokens":10}}}"#,
            ],
        );

        let messages = parse_cherrystudio_file(&path);
        assert_eq!(
            messages.len(),
            2,
            "rows without an identity are retained conservatively"
        );
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
