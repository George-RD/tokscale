//! fx (vercel-labs/fx) session parser
//!
//! Parses per-session usage snapshots from
//! `~/.fx/sessions/<sessionId>/usage-v2.json`, paired with the sibling
//! `session.json` (workspace root + authoritative timestamps) and the shared
//! `~/.fx/sessions/index.json` (human-readable session title).
//!
//! fx aggregates per-request token usage into one snapshot per session, so this
//! parser emits one `UnifiedMessage` per (session × model) entry — the same
//! session-level shape as other aggregate integrations (Kilo, Goose, Mux).
//! A session whose snapshot carries only the top-level aggregates (empty
//! `models`) is attributed to a synthetic `fx-unknown` model instead of being
//! dropped, so the session totals are never silently lost.
//!
//! The global `~/.fx/usage.jsonl` stream also exists (one `generation` record
//! per request) but carries no session id or workspace, so it is intentionally
//! not scanned here.
//!
//! Cache split. The two sidecars are read back differently because they sit at
//! different scopes. `session.json` is per-session, so it is a related file of
//! the snapshot's cache fingerprint ([`fx_session_meta_path`]) and a
//! sidecar-only edit invalidates exactly that one entry. `index.json` is one
//! file shared by every session, so it is deliberately *not* in any
//! fingerprint — appending a new session to it would otherwise invalidate every
//! other session's entry. Its title is resolved after the cache instead, by
//! [`apply_session_titles`], which reads the index once per scan and writes the
//! title onto freshly parsed and cache-served messages alike.

use super::utils::{file_modified_timestamp_ms, read_file_or_none};
use super::{normalize_workspace_key, workspace_label_from_key, CostSource, UnifiedMessage};
use crate::{provider_identity, TokenBreakdown};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const CLIENT_ID: &str = "fx";
// `fx-unknown` follows the `<client>-unknown` convention (trae.rs) for
// session usage whose per-model breakdown is unavailable: the session totals
// are still attributed instead of disappearing into an empty Model cell.
const UNKNOWN_MODEL: &str = "fx-unknown";

#[derive(Debug, Deserialize)]
struct FxUsageFile {
    #[allow(dead_code)]
    schema_version: Option<u32>,
    #[allow(dead_code)]
    session_id: Option<String>,
    #[serde(default)]
    snapshot: Option<FxSnapshot>,
}

#[derive(Debug, Default, Deserialize)]
struct FxSnapshot {
    #[allow(dead_code)]
    schema_version: Option<u32>,
    // Top-level session aggregates (session_usage.zig `Snapshot`) always
    // accompany `models`; the per-model entries are the breakdown of these
    // totals. They back the synthetic fallback below.
    #[serde(default)]
    total_cost: f64,
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_read_tokens: i64,
    #[serde(default)]
    cache_write_tokens: i64,
    #[serde(default)]
    reasoning_tokens: Option<i64>,
    #[serde(default)]
    request_count: Option<i64>,
    #[serde(default)]
    models: Vec<FxModelUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct FxModelUsage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    total_cost: f64,
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_read_tokens: i64,
    #[serde(default)]
    cache_write_tokens: i64,
    // Nullable in the wire schema (`writeOptionalU64`), so these must be
    // `Option`; an explicit JSON `null` would otherwise fail the whole file
    // parse and drop the session.
    #[serde(default)]
    reasoning_tokens: Option<i64>,
    #[serde(default)]
    request_count: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct FxSessionMeta {
    #[allow(dead_code)]
    id: Option<String>,
    #[serde(default)]
    workspace_root: Option<String>,
    #[serde(default)]
    created_at_ms: Option<i64>,
    #[serde(default)]
    updated_at_ms: Option<i64>,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = read_file_or_none(path)?;
    serde_json::from_slice(&bytes).ok()
}

/// The sibling `session.json` a snapshot's parse reads for the workspace root
/// and the authoritative timestamps.
///
/// Always returns a path, including for a session that has no `session.json`
/// yet: the cache records the absence so a later creation still invalidates the
/// entry instead of freezing the mtime fallback timestamp forever.
pub(crate) fn fx_session_meta_path(usage_path: &Path) -> PathBuf {
    usage_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("session.json")
}

/// Read `<sessions>/index.json` (`{"sessions":[{"id","title",...}]}`) into
/// `id -> title`. A missing, unreadable or malformed index contributes nothing;
/// the Sessions tab then shows the session id.
fn collect_index_titles(sessions_dir: &Path, titles: &mut HashMap<String, String>) {
    let Some(value) = read_json::<serde_json::Value>(&sessions_dir.join("index.json")) else {
        return;
    };
    let Some(sessions) = value.get("sessions").and_then(|s| s.as_array()) else {
        return;
    };
    for entry in sessions {
        let Some(id) = entry.get("id").and_then(|id| id.as_str()) else {
            continue;
        };
        let title = entry
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim();
        if title.is_empty() {
            continue;
        }
        titles.insert(id.to_string(), title.to_string());
    }
}

/// Attach human-readable session titles to an fx lane's messages.
///
/// Runs after the source-message cache, not inside [`parse_fx_file`], because
/// `index.json` is shared by every fx session: making it part of a per-session
/// cache fingerprint would let one new session invalidate every other session's
/// entry. Resolving it here keeps a title-only edit free — nothing is
/// invalidated, the index is parsed once per scan rather than once per session,
/// and a cache-served message still gets the current title.
///
/// The title is assigned, not filled in: a message restored from the cache
/// carries whatever title the index held when it was written, so an edited or
/// removed title has to be able to overwrite it back to `None`.
///
/// `usage_paths` are the scanned `usage-v2.json` paths; their grandparent is
/// the sessions directory holding the index. Only fx messages are touched, so
/// passing a wider slice cannot clear another client's titles.
pub fn apply_session_titles(usage_paths: &[PathBuf], messages: &mut [UnifiedMessage]) {
    if messages.is_empty() {
        return;
    }

    let mut titles: HashMap<String, String> = HashMap::new();
    let mut seen_dirs: HashSet<&Path> = HashSet::new();
    for path in usage_paths {
        let Some(sessions_dir) = path.parent().and_then(Path::parent) else {
            continue;
        };
        if !seen_dirs.insert(sessions_dir) {
            continue;
        }
        collect_index_titles(sessions_dir, &mut titles);
    }

    for message in messages {
        if message.client != CLIENT_ID {
            continue;
        }
        message.session_title = titles.get(&message.session_id).cloned();
    }
}

/// Split a provider-prefixed fx model id (`zai/glm-5.2`) into `(provider,
/// model)` without dropping either half. Models without a `/` prefix are kept
/// whole and the provider is inferred downstream.
fn split_model(raw: &str) -> (Option<String>, String) {
    match raw.split_once('/') {
        Some((provider, rest)) if !provider.is_empty() && !rest.is_empty() => {
            (Some(provider.to_string()), rest.to_string())
        }
        _ => (None, raw.to_string()),
    }
}

/// Parse one `usage-v2.json` file (a single fx session).
/// Returns one `UnifiedMessage` per model with non-zero recorded usage, or one
/// synthetic `fx-unknown` message from the top-level session aggregates when
/// no per-model entry produced a message.
pub fn parse_fx_file(path: &Path) -> Vec<UnifiedMessage> {
    let Some(bytes) = read_file_or_none(path) else {
        return Vec::new();
    };
    let file: FxUsageFile = match serde_json::from_slice(&bytes) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let Some(snapshot) = file.snapshot else {
        return Vec::new();
    };

    let session_dir = path.parent();

    let session_id = file
        .session_id
        .filter(|s| !s.is_empty())
        .or_else(|| {
            session_dir
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .unwrap_or_default();

    // Sibling `session.json` carries the workspace root and authoritative
    // timestamps; fall back to the usage file's mtime when absent.
    let meta: Option<FxSessionMeta> = read_json(&fx_session_meta_path(path));
    let workspace_root = meta.as_ref().and_then(|m| m.workspace_root.clone());
    let timestamp_ms = meta
        .as_ref()
        .and_then(|m| m.updated_at_ms.or(m.created_at_ms));
    let fallback_timestamp = file_modified_timestamp_ms(path);
    let timestamp = timestamp_ms.unwrap_or(fallback_timestamp);

    let workspace_key = workspace_root.as_deref().and_then(normalize_workspace_key);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);

    let mut messages = Vec::new();
    for model_usage in &snapshot.models {
        let raw_model = model_usage.model.as_deref().unwrap_or(UNKNOWN_MODEL);
        let tokens = TokenBreakdown {
            input: model_usage.input_tokens.max(0),
            output: model_usage.output_tokens.max(0),
            cache_read: model_usage.cache_read_tokens.max(0),
            cache_write: model_usage.cache_write_tokens.max(0),
            reasoning: model_usage.reasoning_tokens.unwrap_or(0).max(0),
        };
        if tokens.total() == 0 {
            continue;
        }

        let (provider, model_id) = split_model(raw_model);
        let provider_id = provider
            .as_deref()
            .and_then(provider_identity::canonical_provider)
            .or_else(|| {
                provider_identity::inferred_provider_from_model(&model_id).map(str::to_string)
            })
            .unwrap_or_else(|| "zai".to_string());

        let dedup_key = format!("fx:{session_id}:{model_id}");

        messages.push(UnifiedMessage {
            client: CLIENT_ID.to_string(),
            model_id,
            provider_id,
            session_id: session_id.clone(),
            workspace_key: workspace_key.clone(),
            workspace_label: workspace_label.clone(),
            timestamp,
            date: String::new(),
            tokens,
            cost: model_usage.total_cost.max(0.0),
            cost_source: CostSource::ProviderReported,
            duration_ms: None,
            message_count: model_usage.request_count.unwrap_or(0).max(0) as i32,
            agent: None,
            dedup_key: Some(dedup_key),
            // Resolved after the cache by `apply_session_titles`; see the
            // module docs for why the shared index stays out of the parse.
            session_title: None,
            is_turn_start: false,
            model_attribution_conflicted: false,
        });
    }

    // fx also writes session-level aggregates at the top of the snapshot,
    // alongside the per-model breakdown. When no per-model entry produced a
    // message (empty `models`, or every entry with zero tokens), attribute the
    // session totals to a synthetic unknown model instead of dropping the
    // session's usage silently.
    if messages.is_empty() {
        let tokens = TokenBreakdown {
            input: snapshot.input_tokens.max(0),
            output: snapshot.output_tokens.max(0),
            cache_read: snapshot.cache_read_tokens.max(0),
            cache_write: snapshot.cache_write_tokens.max(0),
            reasoning: snapshot.reasoning_tokens.unwrap_or(0).max(0),
        };
        let request_count = snapshot.request_count.unwrap_or(0).max(0);
        let cost = snapshot.total_cost.max(0.0);
        if tokens.total() > 0 || request_count > 0 || cost > 0.0 {
            messages.push(UnifiedMessage {
                client: CLIENT_ID.to_string(),
                model_id: UNKNOWN_MODEL.to_string(),
                provider_id: "zai".to_string(),
                session_id: session_id.clone(),
                workspace_key: workspace_key.clone(),
                workspace_label: workspace_label.clone(),
                timestamp,
                date: String::new(),
                tokens,
                cost,
                cost_source: CostSource::ProviderReported,
                duration_ms: None,
                message_count: request_count as i32,
                agent: None,
                dedup_key: Some(format!("fx:{session_id}:{UNKNOWN_MODEL}")),
                session_title: None,
                is_turn_start: false,
                model_attribution_conflicted: false,
            });
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TokenBreakdown;

    fn write_file(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session = sessions.join("sess-123");
        std::fs::create_dir_all(&session).unwrap();
        (dir, session)
    }

    #[test]
    fn parses_provider_prefixed_model_into_one_message() {
        let (dir, session) = fixture();
        write_file(
            &session,
            "session.json",
            r#"{"workspace_root":"/Users/alice/repo","updated_at_ms":1787196905040}"#,
        );
        write_file(
            &dir.path().join("sessions"),
            "index.json",
            r#"{"schema_version":3,"sessions":[{"id":"sess-123","workspace_root":"/Users/alice/repo","title":"Setup CI"}]}"#,
        );
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{
              "schema_version":1,
              "session_id":"sess-123",
              "snapshot":{
                "schema_version":2,
                "total_cost":0.01,
                "request_count":2,
                "models":[{"model":"zai/glm-5.2","total_cost":0.01,"input_tokens":1539,"output_tokens":441,"cache_read_tokens":1069,"cache_write_tokens":7,"reasoning_tokens":3,"request_count":2}]
              }
            }"#,
        );

        let mut messages = parse_fx_file(&usage);
        // The parse leaves the title unset; the lane resolves it from the
        // shared index once per scan, after the cache.
        assert_eq!(messages[0].session_title, None);
        apply_session_titles(std::slice::from_ref(&usage), &mut messages);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.client, "fx");
        assert_eq!(msg.session_id, "sess-123");
        assert_eq!(msg.model_id, "glm-5.2");
        assert_eq!(msg.provider_id, "zai");
        assert_eq!(msg.workspace_key.as_deref(), Some("/Users/alice/repo"));
        assert_eq!(msg.session_title.as_deref(), Some("Setup CI"));
        assert_eq!(msg.timestamp, 1787196905040);
        assert_eq!(
            msg.tokens,
            TokenBreakdown {
                input: 1539,
                output: 441,
                cache_read: 1069,
                cache_write: 7,
                reasoning: 3,
            }
        );
        assert!((msg.cost - 0.01).abs() < 1e-9);
        assert_eq!(msg.message_count, 2);
        assert_eq!(msg.cost_source, CostSource::ProviderReported);
    }

    #[test]
    fn skips_session_with_no_usage() {
        let (_dir, session) = fixture();
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{"schema_version":1,"session_id":"empty","snapshot":{"models":[],"request_count":0,"total_cost":0}}"#,
        );
        assert!(parse_fx_file(&usage).is_empty());
    }

    #[test]
    fn falls_back_to_synthetic_unknown_model_from_top_level_aggregates() {
        let (dir, session) = fixture();
        write_file(
            &session,
            "session.json",
            r#"{"workspace_root":"/Users/alice/repo","updated_at_ms":1787196905040}"#,
        );
        write_file(
            &dir.path().join("sessions"),
            "index.json",
            r#"{"schema_version":3,"sessions":[{"id":"sess-456","workspace_root":"/Users/alice/repo","title":"Refactor CLI"}]}"#,
        );
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{
              "schema_version":1,
              "session_id":"sess-456",
              "snapshot":{
                "schema_version":2,
                "total_cost":0.014,
                "input_tokens":2000,
                "output_tokens":800,
                "cache_read_tokens":500,
                "cache_write_tokens":10,
                "reasoning_tokens":120,
                "request_count":3,
                "models":[]
              }
            }"#,
        );

        let mut messages = parse_fx_file(&usage);
        apply_session_titles(std::slice::from_ref(&usage), &mut messages);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.client, "fx");
        assert_eq!(msg.session_id, "sess-456");
        assert_eq!(msg.model_id, "fx-unknown");
        assert_eq!(msg.provider_id, "zai");
        assert_eq!(msg.workspace_key.as_deref(), Some("/Users/alice/repo"));
        assert_eq!(msg.session_title.as_deref(), Some("Refactor CLI"));
        assert_eq!(msg.timestamp, 1787196905040);
        assert_eq!(
            msg.tokens,
            TokenBreakdown {
                input: 2000,
                output: 800,
                cache_read: 500,
                cache_write: 10,
                reasoning: 120,
            }
        );
        assert!((msg.cost - 0.014).abs() < 1e-9);
        assert_eq!(msg.message_count, 3);
        assert_eq!(msg.cost_source, CostSource::ProviderReported);
        assert_eq!(msg.dedup_key.as_deref(), Some("fx:sess-456:fx-unknown"));
    }

    #[test]
    fn top_level_aggregates_do_not_double_count_when_models_present() {
        let (_dir, session) = fixture();
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{
              "schema_version":1,
              "session_id":"s",
              "snapshot":{
                "total_cost":0.02,
                "input_tokens":3000,
                "output_tokens":1000,
                "request_count":4,
                "models":[{"model":"zai/glm-5.2","total_cost":0.02,"input_tokens":3000,"output_tokens":1000,"request_count":4}]
              }
            }"#,
        );
        let messages = parse_fx_file(&usage);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "glm-5.2");
    }

    #[test]
    fn tolerates_null_reasoning_and_request_count_fields() {
        // The wire schema serializes absent `reasoning_tokens`/`request_count`
        // as JSON `null`. An explicit `null` must not fail the file parse and
        // drop the session.
        let (_dir, session) = fixture();
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{"schema_version":1,"session_id":"s3","snapshot":{"models":[{"model":"anthropic/claude-sonnet-4","total_cost":0.001,"input_tokens":10,"output_tokens":5,"reasoning_tokens":null,"request_count":null}]}}"#,
        );
        let messages = parse_fx_file(&usage);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.reasoning, 0);
        assert_eq!(messages[0].message_count, 0);
    }

    #[test]
    fn skips_model_entry_with_zero_tokens() {
        let (_dir, session) = fixture();
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{"schema_version":1,"session_id":"s","snapshot":{"models":[{"model":"zai/glm-5.2","input_tokens":0,"output_tokens":0}]}}"#,
        );
        assert!(parse_fx_file(&usage).is_empty());
    }

    #[test]
    fn tolerates_missing_sibling_metadata() {
        let (_dir, session) = fixture();
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{"schema_version":1,"session_id":"s2","snapshot":{"models":[{"model":"glm-5.2","input_tokens":10,"output_tokens":5}]}}"#,
        );
        let mut messages = parse_fx_file(&usage);
        apply_session_titles(std::slice::from_ref(&usage), &mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "zai");
        assert_eq!(messages[0].workspace_key, None);
        assert_eq!(messages[0].session_title, None);
    }

    #[test]
    fn apply_session_titles_overwrites_a_cache_served_title() {
        // A message restored from the source-message cache carries the title
        // the index held when the entry was written. A later rename must be
        // able to replace it, and a removed title must be able to clear it --
        // hence assignment rather than fill-if-empty.
        let (dir, session) = fixture();
        let sessions = dir.path().join("sessions");
        write_file(
            &sessions,
            "index.json",
            r#"{"schema_version":3,"sessions":[{"id":"sess-123","title":"Renamed later"}]}"#,
        );
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{"schema_version":1,"session_id":"sess-123","snapshot":{"models":[{"model":"zai/glm-5.2","input_tokens":10,"output_tokens":5}]}}"#,
        );

        let mut messages = parse_fx_file(&usage);
        messages[0].session_title = Some("Stale cached title".to_string());
        apply_session_titles(std::slice::from_ref(&usage), &mut messages);
        assert_eq!(messages[0].session_title.as_deref(), Some("Renamed later"));

        write_file(
            &sessions,
            "index.json",
            r#"{"schema_version":3,"sessions":[{"id":"sess-123","title":"   "}]}"#,
        );
        apply_session_titles(std::slice::from_ref(&usage), &mut messages);
        assert_eq!(
            messages[0].session_title, None,
            "a blank title must clear the cached one, not leave it standing"
        );
    }

    #[test]
    fn apply_session_titles_leaves_other_clients_alone() {
        let (dir, session) = fixture();
        write_file(
            &dir.path().join("sessions"),
            "index.json",
            r#"{"schema_version":3,"sessions":[{"id":"sess-123","title":"Setup CI"}]}"#,
        );
        let usage = write_file(
            &session,
            "usage-v2.json",
            r#"{"schema_version":1,"session_id":"sess-123","snapshot":{"models":[{"model":"zai/glm-5.2","input_tokens":10,"output_tokens":5}]}}"#,
        );

        let mut messages = parse_fx_file(&usage);
        let mut foreign = messages[0].clone();
        foreign.client = "codex".to_string();
        foreign.session_title = Some("Someone else's title".to_string());
        messages.push(foreign);

        apply_session_titles(std::slice::from_ref(&usage), &mut messages);
        assert_eq!(messages[0].session_title.as_deref(), Some("Setup CI"));
        assert_eq!(
            messages[1].session_title.as_deref(),
            Some("Someone else's title")
        );
    }

    #[test]
    fn apply_session_titles_covers_every_session_under_one_index() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let mut usage_paths = Vec::new();
        let mut messages = Vec::new();
        for id in ["sess-a", "sess-b"] {
            let session = sessions.join(id);
            std::fs::create_dir_all(&session).unwrap();
            let usage = write_file(
                &session,
                "usage-v2.json",
                &format!(
                    r#"{{"schema_version":1,"session_id":"{id}","snapshot":{{"models":[{{"model":"zai/glm-5.2","input_tokens":10,"output_tokens":5}}]}}}}"#
                ),
            );
            messages.extend(parse_fx_file(&usage));
            usage_paths.push(usage);
        }
        write_file(
            &sessions,
            "index.json",
            r#"{"schema_version":3,"sessions":[{"id":"sess-a","title":"First"},{"id":"sess-b","title":"Second"}]}"#,
        );

        apply_session_titles(&usage_paths, &mut messages);
        let mut titles: Vec<Option<&str>> = messages
            .iter()
            .map(|m| m.session_title.as_deref())
            .collect();
        titles.sort();
        assert_eq!(titles, vec![Some("First"), Some("Second")]);
    }

    #[test]
    fn fx_session_meta_path_points_at_the_sibling_metadata() {
        let (_dir, session) = fixture();
        let usage = session.join("usage-v2.json");
        assert_eq!(fx_session_meta_path(&usage), session.join("session.json"));
    }
}
