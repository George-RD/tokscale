//! GitHub Copilot Desktop SQLite parser.
//!
//! The macOS desktop app stores aggregate token totals in `~/.copilot/data.db`
//! and per-session event metadata in `~/.copilot/session-state/{session_id}`.

use super::utils::lossy_lines;
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::provider_identity::inferred_provider_from_model;
use chrono::{DateTime, NaiveDateTime};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::io::BufReader;
use std::path::Path;
use tracing::warn;

#[derive(Debug)]
struct CopilotDesktopSessionRow {
    id: String,
    model: Option<String>,
    total_input_tokens: i64,
    total_output_tokens: i64,
    total_cached_tokens: i64,
    total_reasoning_tokens: i64,
    created_at: Option<String>,
}

#[derive(Debug, Default)]
struct SessionStateMetadata {
    model: Option<String>,
    cwd: Option<String>,
    shutdowns: Vec<ShutdownUsage>,
}

/// One model's usage from a single `session.shutdown` record.
///
/// These carry their own timestamp, which is the only per-run timing the
/// desktop app exposes: the `sessions` row has a lifetime total and an
/// immutable `created_at`.
#[derive(Debug)]
struct ShutdownUsage {
    timestamp_ms: i64,
    model: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
}

pub fn parse_copilot_desktop_db(db_path: &Path) -> Vec<UnifiedMessage> {
    let conn = match Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to open Copilot Desktop database"
            );
            return Vec::new();
        }
    };

    let mut stmt = match conn.prepare(
        r#"
        SELECT
            id,
            title,
            model,
            total_input_tokens,
            total_output_tokens,
            total_cached_tokens,
            total_reasoning_tokens,
            total_nano_aiu,
            created_at
        FROM sessions
        WHERE total_input_tokens > 0
           OR total_output_tokens > 0
           OR total_cached_tokens > 0
           OR total_reasoning_tokens > 0
        "#,
    ) {
        Ok(stmt) => stmt,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to prepare Copilot Desktop sessions query"
            );
            return Vec::new();
        }
    };

    let rows = match stmt.query_map([], |row| {
        Ok(CopilotDesktopSessionRow {
            id: row.get(0)?,
            model: row.get(2)?,
            total_input_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            total_output_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            total_cached_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            total_reasoning_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            created_at: row.get(8)?,
        })
    }) {
        Ok(rows) => rows,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to execute Copilot Desktop sessions query"
            );
            return Vec::new();
        }
    };

    rows.flat_map(|row| match row {
        Ok(row) => session_row_to_messages(db_path, row),
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to decode Copilot Desktop session row"
            );
            Vec::new()
        }
    })
    .collect()
}

/// Turn one `sessions` row into the messages its usage actually belongs to.
///
/// The row holds a lifetime total against an immutable `created_at`, so
/// emitting it as-is re-dated every later turn to the day the session was
/// opened: that day grew on every rescan and the days the tokens were really
/// spent on received none of them (#962).
///
/// `session.shutdown` records carry their own timestamp and a per-model
/// breakdown, so each one is emitted at its own time and under its own model.
/// Whatever they do not account for — a run that died before writing its
/// shutdown, or a session recorded by the CLI rather than the desktop app —
/// stays on `created_at` under the row's original dedup key, so the row
/// remains the authority on the all-time total and nothing is dropped.
fn session_row_to_messages(db_path: &Path, row: CopilotDesktopSessionRow) -> Vec<UnifiedMessage> {
    let metadata = read_session_state_metadata(db_path, &row.id);
    let fallback_model = metadata
        .model
        .as_deref()
        .or(row.model.as_deref())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("auto")
        .to_string();

    let created_at_ms = row
        .created_at
        .as_deref()
        .and_then(parse_iso8601_timestamp_ms)
        .unwrap_or_else(|| {
            warn!(
                session_id = %row.id,
                created_at = ?row.created_at,
                "Copilot Desktop session has unparseable created_at; defaulting to 0"
            );
            0
        });

    let workspace_key = metadata.cwd.as_deref().and_then(normalize_workspace_key);
    let build = |model_id: String, timestamp_ms: i64, tokens, dedup_key: String| {
        let provider_id = inferred_provider_from_model(&model_id)
            .unwrap_or("github-copilot")
            .to_string();
        let mut message = UnifiedMessage::new_with_dedup(
            "copilot",
            model_id,
            provider_id,
            row.id.clone(),
            timestamp_ms,
            tokens,
            0.0,
            Some(dedup_key),
        );
        if let Some(workspace_key) = workspace_key.clone() {
            let workspace_label = workspace_label_from_key(&workspace_key);
            message.set_workspace(Some(workspace_key), workspace_label);
        }
        message
    };

    let mut messages = Vec::with_capacity(metadata.shutdowns.len() + 1);
    for (index, shutdown) in metadata.shutdowns.iter().enumerate() {
        let model_id = match shutdown.model.trim() {
            "" | "auto" => fallback_model.clone(),
            model => model.to_string(),
        };
        messages.push(build(
            model_id,
            shutdown.timestamp_ms,
            // Copilot reports input tokens inclusive of cache reads (same
            // convention as the OTEL exporter that feeds this same session
            // data). Reuse the shared normalizer so the desktop-DB and OTEL
            // paths never diverge and additive pricing does not double-charge
            // the cached portion.
            super::copilot::normalize_input_tokens(
                shutdown.input,
                shutdown.output,
                shutdown.cache_read,
                shutdown.cache_write,
                shutdown.reasoning,
            ),
            format!(
                "copilot-desktop:{}:shutdown:{index}:{}",
                row.id, shutdown.model
            ),
        ));
    }

    let consumed = |pick: fn(&ShutdownUsage) -> i64| -> i64 {
        metadata
            .shutdowns
            .iter()
            .map(pick)
            .fold(0i64, i64::saturating_add)
    };
    // The row's own cache-write column does not exist, so the shutdown records
    // are the only source for that bucket and there is nothing to reconcile.
    let residual = super::copilot::normalize_input_tokens(
        (row.total_input_tokens - consumed(|usage| usage.input)).max(0),
        (row.total_output_tokens - consumed(|usage| usage.output)).max(0),
        (row.total_cached_tokens - consumed(|usage| usage.cache_read)).max(0),
        0,
        (row.total_reasoning_tokens - consumed(|usage| usage.reasoning)).max(0),
    );

    if messages.is_empty() || residual.total() > 0 {
        messages.push(build(
            fallback_model,
            created_at_ms,
            residual,
            format!("copilot-desktop:{}", row.id),
        ));
    }

    messages
}

fn read_session_state_metadata(db_path: &Path, session_id: &str) -> SessionStateMetadata {
    let Some(copilot_root) = db_path.parent() else {
        return SessionStateMetadata::default();
    };
    let events_path = copilot_root
        .join("session-state")
        .join(session_id)
        .join("events.jsonl");

    read_events_metadata(&events_path)
}

fn read_events_metadata(events_path: &Path) -> SessionStateMetadata {
    let file = match std::fs::File::open(events_path) {
        Ok(file) => file,
        Err(_) => return SessionStateMetadata::default(),
    };

    let mut metadata = SessionStateMetadata::default();
    for line in lossy_lines(BufReader::new(file)) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            continue;
        };

        match event_type {
            "session.start" if metadata.cwd.is_none() => {
                metadata.cwd = event
                    .pointer("/data/context/cwd")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|cwd| !cwd.is_empty())
                    .map(str::to_string);
            }
            "session.model_change" => {
                if let Some(model) = event
                    .pointer("/data/newModel")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty() && model != &"auto")
                {
                    metadata.model = Some(model.to_string());
                }
            }
            "session.shutdown" => collect_shutdown_usage(&event, &mut metadata.shutdowns),
            _ => {}
        }
    }

    metadata
}

fn collect_shutdown_usage(event: &Value, out: &mut Vec<ShutdownUsage>) {
    // The desktop app nests event payloads under `data`; a flat record is
    // accepted too so a shutdown that omits the envelope still reports usage
    // rather than silently contributing nothing.
    let payload = event.get("data").unwrap_or(event);
    // The timestamp lives on the envelope next to `id`/`parentId`, not in the
    // payload, and it is an ISO-8601 string. Reading the payload first only
    // matters for a flat record that has no envelope to read from.
    let Some(timestamp_ms) = event
        .get("timestamp")
        .or_else(|| payload.get("timestamp"))
        .and_then(Value::as_str)
        .and_then(parse_iso8601_timestamp_ms)
    else {
        return;
    };
    let Some(metrics) = payload
        .get("modelMetrics")
        .or_else(|| event.get("modelMetrics"))
        .and_then(Value::as_object)
    else {
        return;
    };

    for (model, entry) in metrics {
        let Some(usage) = entry.get("usage") else {
            continue;
        };
        let read = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0).max(0);
        let shutdown = ShutdownUsage {
            timestamp_ms,
            model: model.clone(),
            input: read("inputTokens"),
            output: read("outputTokens"),
            cache_read: read("cacheReadTokens"),
            cache_write: read("cacheWriteTokens"),
            reasoning: read("reasoningTokens"),
        };
        if shutdown.input == 0
            && shutdown.output == 0
            && shutdown.cache_read == 0
            && shutdown.cache_write == 0
            && shutdown.reasoning == 0
        {
            continue;
        }
        out.push(shutdown);
    }
}

fn parse_iso8601_timestamp_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            // SQLite's default datetime() text form is space-separated and may
            // carry fractional seconds ("2026-07-01 12:34:56.789"); without this
            // branch it fails every parse above and the session lands in 1970.
            NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|timestamp| timestamp.and_utc().timestamp_millis())
        })
        .or_else(|| {
            let numeric = value.parse::<i64>().ok()?;
            // Distinguish seconds vs milliseconds: values < 10 billion are
            // assumed to be Unix seconds (common in SQLite), otherwise millis.
            if numeric > 10_000_000_000 {
                Some(numeric)
            } else {
                Some(numeric.saturating_mul(1000))
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use std::fs::{self, File};
    use std::io::Write;

    fn create_copilot_desktop_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE sessions (
                id TEXT,
                title TEXT,
                session_type TEXT,
                mode TEXT,
                model TEXT,
                total_input_tokens INTEGER,
                total_output_tokens INTEGER,
                total_cached_tokens INTEGER,
                total_reasoning_tokens INTEGER,
                total_nano_aiu INTEGER,
                created_at TEXT,
                agent TEXT,
                provider_id TEXT
            );
            "#,
        )
        .unwrap();
        conn
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        model: &str,
        input: i64,
        output: i64,
        cached: i64,
        reasoning: i64,
    ) {
        conn.execute(
            r#"
            INSERT INTO sessions (
                id, title, session_type, mode, model,
                total_input_tokens, total_output_tokens, total_cached_tokens,
                total_reasoning_tokens, total_nano_aiu, created_at, agent, provider_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            "#,
            params![
                id,
                "Test session",
                "chat",
                "agent",
                model,
                input,
                output,
                cached,
                reasoning,
                0_i64,
                "2026-07-01T12:34:56Z",
                "github.copilot.default",
                "github-copilot"
            ],
        )
        .unwrap();
    }

    fn write_events(root: &Path, session_id: &str, lines: &[&str]) {
        let events_dir = root.join("session-state").join(session_id);
        fs::create_dir_all(&events_dir).unwrap();
        let mut file = File::create(events_dir.join("events.jsonl")).unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
    }

    #[test]
    fn parse_copilot_desktop_db_reads_token_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.1-codex", 100, 50, 25, 10);
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.client, "copilot");
        assert_eq!(message.model_id, "gpt-5.1-codex");
        assert_eq!(message.provider_id, "openai");
        assert_eq!(message.session_id, "session-1");
        assert_eq!(message.timestamp, 1_782_909_296_000);
        // total_input_tokens is inclusive of cache reads, so the cached portion
        // (25) is normalized out of input: 100 - 25 = 75.
        assert_eq!(message.tokens.input, 75);
        assert_eq!(message.tokens.output, 50);
        assert_eq!(message.tokens.cache_read, 25);
        assert_eq!(message.tokens.cache_write, 0);
        assert_eq!(message.tokens.reasoning, 10);
        assert_eq!(
            message.dedup_key.as_deref(),
            Some("copilot-desktop:session-1")
        );
    }

    #[test]
    fn parse_copilot_desktop_db_skips_zero_token_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.1-codex", 0, 0, 0, 0);
        drop(conn);

        assert!(parse_copilot_desktop_db(&db_path).is_empty());
    }

    #[test]
    fn parse_copilot_desktop_db_enriches_model_and_workspace_from_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 50, 0, 0);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.start","data":{"context":{"cwd":"/Users/alice/project"}}}"#,
                r#"{"type":"session.model_change","data":{"newModel":"claude-sonnet-4-5"}}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.model_id, "claude-sonnet-4-5");
        assert_eq!(message.provider_id, "anthropic");
        assert_eq!(message.workspace_label.as_deref(), Some("project"));
    }

    #[test]
    fn keeps_reading_events_after_an_undecodable_line() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 50, 0, 0);
        drop(conn);

        let events_dir = dir.path().join("session-state").join("session-1");
        fs::create_dir_all(&events_dir).unwrap();
        let mut fixture = Vec::new();
        fixture.extend_from_slice(
            br#"{"type":"session.start","data":{"context":{"cwd":"/Users/alice/project"}}}"#,
        );
        fixture.push(b'\n');
        // A lone 0xff can never appear in valid UTF-8, so `BufRead::lines()`
        // reports this line as `InvalidData` and `map_while(Result::ok)` would
        // treat it as end of file, losing the model change below it.
        fixture.extend_from_slice(b"{\"type\":\"session.note\",\"data\":\"\xff\xfe\"}\n");
        fixture.extend_from_slice(
            br#"{"type":"session.model_change","data":{"newModel":"claude-sonnet-4-5"}}"#,
        );
        fixture.push(b'\n');
        fs::write(events_dir.join("events.jsonl"), &fixture).unwrap();

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4-5");
        assert_eq!(messages[0].provider_id, "anthropic");
    }

    #[test]
    fn parse_copilot_desktop_db_uses_github_copilot_provider_for_auto() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 100, 0, 0, 0);
        drop(conn);

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "github-copilot");
    }

    /// Regression (#962): the row carries a lifetime total and an immutable
    /// `created_at`, so every rescan grew the creation day and gave the days
    /// the tokens were actually spent on nothing. `session.shutdown` records
    /// carry their own timestamp, so usage lands on the day it happened.
    #[test]
    fn shutdown_events_attribute_usage_to_their_own_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.1-codex", 100, 50, 25, 10);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.shutdown","data":{"shutdownType":"routine","modelMetrics":{"gpt-5.1-codex":{"requests":{"count":1,"cost":1},"usage":{"inputTokens":100,"outputTokens":50,"cacheReadTokens":25,"cacheWriteTokens":0,"reasoningTokens":10}}}},"id":"9f2c6f0e-1d5a-4a7e-9b30-2c8d4e6f1a02","timestamp":"2026-07-02T00:00:00.000Z","parentId":"9f2c6f0e-1d5a-4a7e-9b30-2c8d4e6f1a01"}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1, "the row total is fully accounted for");
        let message = &messages[0];
        assert_eq!(
            message.timestamp, 1_782_950_400_000,
            "usage belongs to the shutdown day, not the creation day"
        );
        assert_eq!(message.model_id, "gpt-5.1-codex");
        assert_eq!(message.tokens.input, 75);
        assert_eq!(message.tokens.output, 50);
        assert_eq!(message.tokens.cache_read, 25);
        assert_eq!(message.tokens.reasoning, 10);
    }

    /// Whatever the shutdown records do not account for still has to be kept,
    /// so the row stays the authority on the all-time total when a run dies
    /// before it can write its shutdown.
    #[test]
    fn usage_beyond_the_shutdown_events_stays_at_session_creation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.1-codex", 200, 100, 50, 20);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.shutdown","data":{"shutdownType":"routine","modelMetrics":{"gpt-5.1-codex":{"requests":{"count":1,"cost":1},"usage":{"inputTokens":100,"outputTokens":50,"cacheReadTokens":25,"cacheWriteTokens":0,"reasoningTokens":10}}}},"id":"9f2c6f0e-1d5a-4a7e-9b30-2c8d4e6f1a02","timestamp":"2026-07-02T00:00:00.000Z","parentId":"9f2c6f0e-1d5a-4a7e-9b30-2c8d4e6f1a01"}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 2);
        let residual = messages
            .iter()
            .find(|message| message.timestamp == 1_782_909_296_000)
            .expect("the unaccounted remainder stays on the creation day");
        assert_eq!(residual.tokens.input, 75);
        assert_eq!(residual.tokens.output, 50);
        assert_eq!(residual.tokens.cache_read, 25);
        assert_eq!(residual.tokens.reasoning, 10);
        assert_eq!(
            residual.dedup_key.as_deref(),
            Some("copilot-desktop:session-1"),
            "the remainder keeps the row's own dedup key"
        );

        let total_input: i64 = messages.iter().map(|message| message.tokens.input).sum();
        assert_eq!(total_input, 150, "the row total is preserved exactly");
    }

    /// The `sessions` table has no cache-write column, so that bucket was
    /// hardcoded to zero. The shutdown records do carry it.
    #[test]
    fn shutdown_events_recover_cache_write_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.1-codex", 100, 50, 25, 10);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.shutdown","data":{"shutdownType":"routine","modelMetrics":{"gpt-5.1-codex":{"requests":{"count":1,"cost":1},"usage":{"inputTokens":100,"outputTokens":50,"cacheReadTokens":25,"cacheWriteTokens":7,"reasoningTokens":10}}}},"id":"9f2c6f0e-1d5a-4a7e-9b30-2c8d4e6f1a02","timestamp":"2026-07-02T00:00:00.000Z","parentId":"9f2c6f0e-1d5a-4a7e-9b30-2c8d4e6f1a01"}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);

        let shutdown = messages
            .iter()
            .find(|message| message.timestamp == 1_782_950_400_000)
            .expect("shutdown message");
        assert_eq!(shutdown.tokens.cache_write, 7);
    }

    /// `modelMetrics` is keyed by model, which attributes each model's usage
    /// exactly instead of letting the last `session.model_change` claim the
    /// whole session.
    #[test]
    fn shutdown_events_split_usage_per_model() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "auto", 300, 60, 0, 0);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.shutdown","data":{"shutdownType":"routine","modelMetrics":{"gpt-5.1-codex":{"usage":{"inputTokens":100,"outputTokens":20}},"claude-sonnet-4-5":{"usage":{"inputTokens":200,"outputTokens":40}}}},"id":"9f2c6f0e-1d5a-4a7e-9b30-2c8d4e6f1a02","timestamp":"2026-07-02T00:00:00.000Z","parentId":"9f2c6f0e-1d5a-4a7e-9b30-2c8d4e6f1a01"}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);

        let codex = messages
            .iter()
            .find(|message| message.model_id == "gpt-5.1-codex")
            .expect("codex row");
        let claude = messages
            .iter()
            .find(|message| message.model_id == "claude-sonnet-4-5")
            .expect("claude row");
        assert_eq!(codex.tokens.input, 100);
        assert_eq!(codex.provider_id, "openai");
        assert_eq!(claude.tokens.input, 200);
        assert_eq!(claude.provider_id, "anthropic");
    }

    /// A `session.shutdown` record captured verbatim from a real
    /// `~/.copilot/session-state/<id>/events.jsonl` on macOS (Copilot CLI
    /// 1.0.25), with only the two UUIDs replaced. It pins the shape the desktop
    /// app actually writes: `timestamp` is an ISO-8601 string on the envelope
    /// next to `id`/`parentId`, `modelMetrics` is nested under `data`, and the
    /// usage bucket carries `cacheWriteTokens`. Reading the timestamp from a
    /// `ts` key under `data` finds nothing and drops the record.
    #[test]
    fn real_shutdown_record_attributes_usage_to_its_own_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("data.db");
        let conn = create_copilot_desktop_db(&db_path);
        insert_session(&conn, "session-1", "gpt-5.4", 21_067, 29, 19_968, 22);
        drop(conn);
        write_events(
            dir.path(),
            "session-1",
            &[
                r#"{"type":"session.shutdown","data":{"shutdownType":"routine","totalPremiumRequests":1,"totalApiDurationMs":2970,"sessionStartTime":1776192215193,"codeChanges":{"linesAdded":0,"linesRemoved":0,"filesModified":[]},"modelMetrics":{"gpt-5.4":{"requests":{"count":1,"cost":1},"usage":{"inputTokens":21067,"outputTokens":29,"cacheReadTokens":19968,"cacheWriteTokens":0,"reasoningTokens":22}}},"currentModel":"gpt-5.4","currentTokens":22592,"systemTokens":9923,"conversationTokens":83,"toolDefinitionsTokens":12583},"id":"c1a4b7e2-90d3-4f61-8ba5-7d2e6f0c9134","timestamp":"2026-04-14T18:43:44.922Z","parentId":"5b8f3d10-2c47-4e89-a6f0-11d9c4e78a25"}"#,
            ],
        );

        let messages = parse_copilot_desktop_db(&db_path);

        assert_eq!(messages.len(), 1, "the row total is fully accounted for");
        let message = &messages[0];
        assert_eq!(
            message.timestamp, 1_776_192_224_922,
            "the envelope timestamp is the run's own time, not `created_at`"
        );
        assert_eq!(message.model_id, "gpt-5.4");
        assert_eq!(message.tokens.input, 1_099);
        assert_eq!(message.tokens.output, 29);
        assert_eq!(message.tokens.cache_read, 19_968);
        assert_eq!(message.tokens.reasoning, 22);
    }

    #[test]
    fn parse_iso8601_handles_space_separated_fractional_seconds() {
        // SQLite datetime() text form; must not fall through to the 1970 default.
        let ms = parse_iso8601_timestamp_ms("2026-07-01 12:34:56.789")
            .expect("space + fractional seconds should parse");
        assert_eq!(ms, 1_782_909_296_789);

        // Sibling formats still parse.
        assert_eq!(
            parse_iso8601_timestamp_ms("2026-07-01T12:34:56Z"),
            Some(1_782_909_296_000)
        );
        assert_eq!(
            parse_iso8601_timestamp_ms("2026-07-01 12:34:56"),
            Some(1_782_909_296_000)
        );
        assert_eq!(parse_iso8601_timestamp_ms("not-a-timestamp"), None);
    }
}
