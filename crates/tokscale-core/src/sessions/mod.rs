//! Session parsers for different AI coding assistant formats
//!
//! Each client has its own parser that converts to a unified message format.

pub mod amp;
pub mod antigravity;
pub mod antigravity_cli;
pub mod augment;
pub mod claudecode;
pub mod cline;
pub mod codebuddy;
pub mod codebuff;
pub mod codex;
pub mod commandcode;
pub mod copilot;
pub mod copilot_desktop;
pub mod copilot_vscode;
pub mod crush;
pub mod cursor;
pub mod devin;
pub mod droid;
pub mod freebuff;
pub mod gemini;
pub mod gjc;
pub mod goose;
pub mod grok;
pub mod hermes;
pub mod jcode;
pub mod junie;
pub mod kilo;
pub mod kilocode;
pub mod kimchi;
pub mod kimi;
pub mod kiro;
pub mod micode;
pub mod mux;
pub mod openclaw;
pub mod opencode;
pub mod opencodereview;
pub mod pi;
pub mod prime_agent;
pub mod qwen;
pub mod reasonix;
pub mod roocode;
pub mod senpi;
pub mod synthetic;
pub(crate) mod tencent_buddy;
pub mod trae;
pub(crate) mod utils;
pub mod warp;
pub mod workbuddy;
pub mod zcode;
pub mod zed;

use std::path::Path;

use crate::TokenBreakdown;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CostSource {
    #[default]
    Unknown,
    ProviderReported,
    Estimated,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UnifiedMessage {
    pub client: String,
    pub model_id: String,
    pub provider_id: String,
    pub session_id: String,
    pub workspace_key: Option<String>,
    pub workspace_label: Option<String>,
    pub timestamp: i64,
    pub date: String,
    pub tokens: TokenBreakdown,
    pub cost: f64,
    #[serde(default)]
    pub cost_source: CostSource,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    #[serde(default = "default_message_count")]
    pub message_count: i32,
    pub agent: Option<String>,
    pub dedup_key: Option<String>,
    /// Human-readable session title/name when the source client stores one
    /// (e.g. OpenCode's `session.title` column). `None` for clients that
    /// don't record a title; the Sessions tab falls back to showing just
    /// the session ID in that case.
    #[serde(default)]
    pub session_title: Option<String>,
    /// True if this message is the first assistant response after a user turn.
    /// Used to count user interaction turns (as opposed to API message count).
    #[serde(default)]
    pub is_turn_start: bool,
    /// True when the parser observed conflicting authoritative model evidence.
    /// Such rows must remain unpriced rather than accepting fallback attribution.
    #[serde(default)]
    pub model_attribution_conflicted: bool,
}

const fn default_message_count() -> i32 {
    1
}

pub fn normalize_agent_name(agent: &str) -> String {
    let cleaned = strip_zero_width_chars(agent);
    let trimmed = cleaned.trim();
    let stripped = strip_agent_prefix(trimmed);
    let canonical = canonicalize_agent_name(stripped);
    let agent_lower = canonical.to_lowercase();

    if agent_lower.contains("plan") {
        if agent_lower.contains("omo") || agent_lower.contains("sisyphus") {
            return "Planner-Sisyphus".to_string();
        }
        return titlecase_agent(&canonical);
    }

    if agent_lower == "omo" || agent_lower == "sisyphus" {
        return "Sisyphus".to_string();
    }

    if agent_lower == "orchestrator-sisyphus" {
        return "Atlas".to_string();
    }

    titlecase_agent(&canonical)
}

pub fn normalize_opencode_agent_name(agent: &str) -> String {
    let cleaned = strip_zero_width_chars(agent);
    let trimmed = cleaned.trim();
    let stripped = strip_agent_prefix(trimmed);
    let canonical = canonicalize_agent_name(stripped);
    let agent_lower = canonical.to_lowercase();

    if let Some(normalized) = normalize_oh_my_opencode_agent_name(&agent_lower) {
        return normalized;
    }

    normalize_agent_name(&canonical)
}

pub fn normalize_copilot_agent_name(agent: &str) -> String {
    // Hardcoded brand name for the default native agent
    if agent.eq_ignore_ascii_case("github.copilot.default") {
        return "GitHub Copilot".to_string();
    }

    // Native github.copilot.* agents: strip prefix, titlecase remainder
    const GITHUB_COPILOT_PREFIX: &str = "github.copilot.";
    if agent
        .get(..GITHUB_COPILOT_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(GITHUB_COPILOT_PREFIX))
    {
        let remainder = &agent[GITHUB_COPILOT_PREFIX.len()..];
        let hyphenated = remainder.replace('.', "-");
        return titlecase_agent(&hyphenated);
    }

    // Plugin:team:slug format — titlecase each colon-separated part, join with ": "
    const PLUGIN_PREFIX: &str = "Plugin:";
    if agent
        .get(..PLUGIN_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(PLUGIN_PREFIX))
    {
        let rest = &agent[PLUGIN_PREFIX.len()..];
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() == 2 {
            let team = titlecase_agent(parts[0]);
            let slug = titlecase_agent(parts[1]);
            return format!("{}: {}", team, slug);
        }
        return titlecase_agent(rest);
    }

    normalize_agent_name(agent)
}

fn normalize_oh_my_opencode_agent_name(agent_lower: &str) -> Option<String> {
    let normalized = match agent_lower {
        // Parenthesized format and dash format
        "sisyphus (ultraworker)"
        | "sisyphus - ultraworker"
        | "sisyphus ultraworker"
        | "sisyphus" => "Sisyphus",
        "hephaestus (deep agent)"
        | "hephaestus - deep agent"
        | "hephaestus deep agent"
        | "hephaestus" => "Hephaestus",
        "prometheus (plan builder)"
        | "prometheus - plan builder"
        | "prometheus plan builder"
        | "prometheus (planner)"
        | "prometheus" => "Prometheus",
        "atlas (plan executor)" | "atlas - plan executor" | "atlas plan executor" | "atlas" => {
            "Atlas"
        }
        "metis (plan consultant)"
        | "metis - plan consultant"
        | "metis plan consultant"
        | "metis" => "Metis",
        "momus (plan critic)"
        | "momus - plan critic"
        | "momus plan critic"
        | "momus (plan reviewer)"
        | "momus" => "Momus",
        "orchestrator-sisyphus" => "Atlas",
        "sisyphus-junior" => "Sisyphus-Junior",
        "planner-sisyphus" => "Planner-Sisyphus",
        _ => return None,
    };

    Some(normalized.to_string())
}

/// Strip zero-width Unicode characters that oh-my-openagent uses as
/// invisible sort-order prefixes (U+200B ZERO WIDTH SPACE, U+200C ZERO
/// WIDTH NON-JOINER, U+200D ZERO WIDTH JOINER, U+FEFF BOM/ZWNBSP).
fn strip_zero_width_chars(s: &str) -> String {
    if !s.contains(['\u{200B}', '\u{200C}', '\u{200D}', '\u{FEFF}']) {
        return s.to_string();
    }
    s.chars()
        .filter(|c| !matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}'))
        .collect()
}

fn strip_agent_prefix(name: &str) -> &str {
    for prefix in &["astrape:", "oh-my-claudecode:", "oh-my-codex:"] {
        if name
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            return &name[prefix.len()..];
        }
    }
    name
}

fn canonicalize_agent_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn titlecase_word(word: &str) -> String {
    match word.to_lowercase().as_str() {
        "ui" => "UI".to_string(),
        "ux" => "UX".to_string(),
        "api" => "API".to_string(),
        _ => {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    upper + &chars.collect::<String>()
                }
            }
        }
    }
}

fn titlecase_agent(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    name.split('-')
        .flat_map(|part| part.split_whitespace())
        .map(titlecase_word)
        .collect::<Vec<_>>()
        .join(" ")
}

impl UnifiedMessage {
    pub fn new(
        client: impl Into<String>,
        model_id: impl Into<String>,
        provider_id: impl Into<String>,
        session_id: impl Into<String>,
        timestamp: i64,
        tokens: TokenBreakdown,
        cost: f64,
    ) -> Self {
        Self::new_full(
            client,
            model_id,
            provider_id,
            session_id,
            timestamp,
            tokens,
            cost,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_agent(
        client: impl Into<String>,
        model_id: impl Into<String>,
        provider_id: impl Into<String>,
        session_id: impl Into<String>,
        timestamp: i64,
        tokens: TokenBreakdown,
        cost: f64,
        agent: Option<String>,
    ) -> Self {
        Self::new_full(
            client,
            model_id,
            provider_id,
            session_id,
            timestamp,
            tokens,
            cost,
            agent,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_dedup(
        client: impl Into<String>,
        model_id: impl Into<String>,
        provider_id: impl Into<String>,
        session_id: impl Into<String>,
        timestamp: i64,
        tokens: TokenBreakdown,
        cost: f64,
        dedup_key: Option<String>,
    ) -> Self {
        Self::new_full(
            client,
            model_id,
            provider_id,
            session_id,
            timestamp,
            tokens,
            cost,
            None,
            dedup_key,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_full(
        client: impl Into<String>,
        model_id: impl Into<String>,
        provider_id: impl Into<String>,
        session_id: impl Into<String>,
        timestamp: i64,
        tokens: TokenBreakdown,
        cost: f64,
        agent: Option<String>,
        dedup_key: Option<String>,
    ) -> Self {
        let date = timestamp_to_date(timestamp);
        Self {
            client: client.into(),
            model_id: model_id.into(),
            provider_id: provider_id.into(),
            session_id: session_id.into(),
            workspace_key: None,
            workspace_label: None,
            timestamp,
            date,
            tokens,
            cost,
            cost_source: CostSource::Unknown,
            duration_ms: None,
            message_count: default_message_count(),
            agent,
            dedup_key,
            session_title: None,
            is_turn_start: false,
            model_attribution_conflicted: false,
        }
    }

    pub fn set_workspace(
        &mut self,
        workspace_key: Option<String>,
        workspace_label: Option<String>,
    ) {
        self.workspace_key = workspace_key;
        self.workspace_label = workspace_label;
    }

    pub(crate) fn refresh_derived_fields(&mut self) {
        self.date = timestamp_to_date(self.timestamp);
    }

    /// Re-derive the day bucket under an explicitly chosen timezone.
    ///
    /// `UnifiedMessage::new` is a constructor called from 92 sites across 42
    /// parser files, so the zone cannot be threaded into it without touching
    /// every one. It does not need to be: `date` is a derived field, already
    /// recomputed from `timestamp` after construction. This lets the one
    /// post-parse pass that holds the user's settings re-key every message at
    /// once, which is the only place the pinned zone is actually known.
    pub(crate) fn rebucket_date(&mut self, timezone: &crate::bucket_tz::BucketTimezone) {
        // A non-positive timestamp is the parsers' "no usable time" sentinel,
        // not an instant before 1970. Re-keying it would move garbage between
        // two equally wrong days, and it is also what bounds the window the
        // auto-pin agreement check has to cover: leaving these alone is what
        // makes `AGREEMENT_WINDOW_START_MS` a real lower bound rather than a
        // convenient one.
        if self.timestamp <= 0 {
            return;
        }

        let key = timezone.day_key(self.timestamp);
        // An unrepresentable instant yields an empty key. Keeping the previous
        // date is wrong by at most the offset between two zones; replacing it
        // with `""` would collapse the message into a bucket that is not a day
        // at all, and that bucket would then be submitted.
        if !key.is_empty() {
            self.date = key;
        }
    }

    pub(crate) fn set_timestamp(&mut self, timestamp: i64) {
        self.timestamp = timestamp;
        self.refresh_derived_fields();
    }

    pub fn mark_provider_reported_cost(&mut self) {
        self.cost_source = CostSource::ProviderReported;
    }

    pub(crate) fn mark_estimated_cost(&mut self) {
        self.cost_source = CostSource::Estimated;
    }

    pub(crate) fn has_authoritative_cost(&self) -> bool {
        self.cost_source == CostSource::ProviderReported
    }
}

pub fn normalize_workspace_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let preserve_unc_prefix = trimmed.starts_with("\\\\") || trimmed.starts_with("//");
    let mut normalized = trimmed.replace('\\', "/");

    if preserve_unc_prefix {
        let body = normalized.trim_start_matches('/');
        let mut collapsed = body.to_string();
        while collapsed.contains("//") {
            collapsed = collapsed.replace("//", "/");
        }
        normalized = format!("//{}", collapsed);
    } else {
        while normalized.contains("//") {
            normalized = normalized.replace("//", "/");
        }
    }

    let minimum_len = if preserve_unc_prefix { 2 } else { 1 };
    if normalized.len() > minimum_len {
        normalized = normalized.trim_end_matches('/').to_string();
    }

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub fn workspace_label_from_key(key: &str) -> Option<String> {
    key.rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
}

/// Marker between a repository and the worktree checked out inside it, e.g.
/// `claude-witness ⑃ lens-backfill-findings`. Worktrees are the common case for
/// agent CLIs that isolate each task, and a repo-only label would render a dozen
/// identical rows.
pub const WORKTREE_SEPARATOR: &str = " ⑃ ";

/// Path segments that mean "everything below me is a worktree, not the repo".
const WORKTREE_MARKERS: [&str; 2] = [".claude/worktrees/", ".git/worktrees/"];

/// The same markers as they appear in a dash-encoded Claude Code slug. A deleted
/// worktree cannot be resolved against the filesystem, but the marker survives
/// verbatim in the slug, so the repo prefix is still recoverable from the string.
const ENCODED_WORKTREE_MARKERS: [&str; 2] = ["--claude-worktrees-", "--git-worktrees-"];

/// Split a dash-encoded slug at its worktree marker into (repo slug, worktree
/// name). Lets rollup and labeling keep working for worktrees whose directories
/// have since been deleted — otherwise those rows keep the raw slug forever.
fn split_encoded_worktree(key: &str) -> Option<(String, String)> {
    for marker in ENCODED_WORKTREE_MARKERS {
        if let Some(index) = key.find(marker) {
            let repo = &key[..index];
            let worktree = &key[index + marker.len()..];
            if !repo.is_empty() && !worktree.is_empty() {
                return Some((repo.to_string(), worktree.to_string()));
            }
        }
    }
    None
}

/// The repository root a workspace key belongs to, with any worktree suffix
/// removed. Returns `None` when the key is not inside a worktree, so callers can
/// tell "already a repo root" from "rolled up to one".
///
/// Only path-shaped keys are handled: clients that store an opaque id (Warp's
/// workspace UUID) have nothing to roll up and are returned untouched.
pub fn workspace_repo_root(key: &str) -> Option<String> {
    for marker in WORKTREE_MARKERS {
        if let Some(index) = key.find(marker) {
            let root = key[..index].trim_end_matches('/');
            if !root.is_empty() {
                return Some(root.to_string());
            }
        }
    }
    None
}

/// Human-readable label for a workspace key: `repo` or `repo ⑃ worktree`.
///
/// The key is whatever the originating client wrote to disk, so this also has to
/// cope with Claude Code's dash-mangled directory slug
/// (`-Users-zetian-devpro-ing-claude-witness`), which carries no `/` to split on
/// and therefore used to render as the entire path — the exact prefix every row
/// shares, so truncation dropped the only distinguishing part.
pub fn workspace_display_label(key: &str) -> Option<String> {
    let path = decode_claude_project_slug(key).unwrap_or_else(|| key.to_string());

    if let Some(root) = workspace_repo_root(&path) {
        let repo = workspace_label_from_key(&root)?;
        return match workspace_label_from_key(&path) {
            Some(worktree) => Some(format!("{repo}{WORKTREE_SEPARATOR}{worktree}")),
            None => Some(repo),
        };
    }

    // Undecodable slug (the directory was deleted): the marker still tells us
    // where the repo ends, so name it from the string rather than giving up and
    // showing the whole mangled path.
    if let Some((repo_slug, worktree)) = split_encoded_worktree(&path) {
        let repo = decode_claude_project_slug(&repo_slug)
            .and_then(|decoded| workspace_label_from_key(&decoded))
            .or_else(|| last_slug_segment(&repo_slug))?;
        return Some(format!("{repo}{WORKTREE_SEPARATOR}{worktree}"));
    }

    workspace_label_from_key(&path)
}

/// Repo identity for a dash-encoded worktree slug whose directory no longer
/// exists, so rollup can still merge it into its repository. Prefers the repo's
/// real path when THAT still resolves, falling back to the repo slug itself —
/// which keeps deleted worktrees of one repo together even then.
pub fn workspace_repo_root_from_slug(key: &str) -> Option<String> {
    let (repo_slug, _) = split_encoded_worktree(key)?;
    Some(decode_claude_project_slug(&repo_slug).unwrap_or(repo_slug))
}

/// Best-effort trailing name of a dash-encoded slug whose directory is gone. The
/// original `/` boundaries are unrecoverable, so this returns the last dash
/// segment — a hint, not an exact path.
fn last_slug_segment(slug: &str) -> Option<String> {
    slug.rsplit('-')
        .find(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
}

/// Claude Code names each project directory after the absolute path it was
/// launched from, replacing every non-alphanumeric byte with `-`. That map is
/// lossy — `/`, `.`, `+` and `-` all collapse to `-` — so it cannot be inverted
/// by string surgery alone. Instead this walks the filesystem, re-applying the
/// same map to real directory names to find which one the slug came from, which
/// makes the recovered path exact rather than a guess.
///
/// Returns `None` for keys that are already real paths, or when no directory on
/// disk matches (a project whose folder has since been deleted or renamed).
pub fn decode_claude_project_slug(key: &str) -> Option<String> {
    // Real paths and Windows keys are already usable; only the slug form starts
    // with the separator-turned-dash and contains no separator of its own.
    if !key.starts_with('-') || key.contains('/') {
        return None;
    }

    resolve_slug_under(Path::new("/"), key)
}

/// Walk `remaining` against the real directories under `dir`.
///
/// A dash in the slug is ambiguous — it may be a `/` boundary, or part of a
/// directory name that genuinely contains `-`, `.` or `+` — so a single greedy
/// pass mis-resolves paths like `claude-witness` (one directory, not two). This
/// consumes one real directory at a time and backtracks when a branch dead-ends,
/// which makes the result exact wherever the directory still exists on disk.
fn resolve_slug_under(dir: &Path, remaining: &str) -> Option<String> {
    if remaining.is_empty() {
        return Some(dir.to_string_lossy().to_string());
    }

    // Longest candidate first: prefer `IngTian.github.io` over a shorter
    // `IngTian` that happens to also exist.
    let mut candidates: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        // `entry.path()` follows symlinks where `entry.file_type()` would not.
        // Symlinked directories are load-bearing here: macOS reaches temp dirs
        // through `/var -> /private/var`, and users symlink project roots.
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| slug_matches_prefix(remaining, name))
        .collect();
    // Ties are real: `a.b` and `a-b` encode identically and nothing on disk
    // distinguishes them, so order deterministically instead of trusting
    // readdir order.
    candidates.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));

    for name in candidates {
        let consumed = slugify_path_segment(&name).len() + 1;
        if let Some(resolved) = resolve_slug_under(&dir.join(&name), &remaining[consumed..]) {
            return Some(resolved);
        }
    }

    None
}

/// Whether `remaining` starts with `-` + the encoded form of `name`, ending on a
/// segment boundary so a directory cannot match half of a longer name.
fn slug_matches_prefix(remaining: &str, name: &str) -> bool {
    let encoded = slugify_path_segment(name);
    let Some(rest) = remaining.strip_prefix('-') else {
        return false;
    };
    let Some(tail) = rest.strip_prefix(encoded.as_str()) else {
        return false;
    };
    tail.is_empty() || tail.starts_with('-')
}

/// Claude Code's forward map: every non-alphanumeric byte becomes `-`.
fn slugify_path_segment(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Convert Unix milliseconds to a local YYYY-MM-DD date string.
fn timestamp_to_date(timestamp_ms: i64) -> String {
    timestamp_to_date_with_timezone(timestamp_ms, &chrono::Local)
}

fn timestamp_to_date_with_timezone<Tz>(timestamp_ms: i64, timezone: &Tz) -> String
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    crate::bucket_tz::format_day_key(timestamp_ms, timezone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn workspace_repo_root_strips_worktree_suffixes() {
        assert_eq!(
            workspace_repo_root("/Users/z/devpro/witness/.claude/worktrees/lens-backfill")
                .as_deref(),
            Some("/Users/z/devpro/witness")
        );
        assert_eq!(
            workspace_repo_root("/Users/z/devpro/witness/.git/worktrees/wt-1").as_deref(),
            Some("/Users/z/devpro/witness")
        );
        // Plain repo checkouts have nothing to roll up.
        assert_eq!(workspace_repo_root("/Users/z/devpro/witness"), None);
        // Opaque, non-path keys (Warp's workspace UUID) must not be mangled.
        assert_eq!(
            workspace_repo_root("9f2c1a04-1e4b-4c3f-a0d1-77b2e5c9aa10"),
            None
        );
    }

    #[test]
    fn workspace_display_label_names_repo_and_worktree() {
        assert_eq!(
            workspace_display_label("/Users/z/devpro/witness/.claude/worktrees/lens-backfill")
                .as_deref(),
            Some("witness ⑃ lens-backfill")
        );
        assert_eq!(
            workspace_display_label("/Users/z/devpro/witness").as_deref(),
            Some("witness")
        );
    }

    #[test]
    fn decode_claude_project_slug_recovers_names_containing_dashes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        // A real name with a literal '-' is the case a greedy split gets wrong:
        // "claude-witness" is ONE directory, not "claude" then "witness".
        std::fs::create_dir_all(root.join("devpro/ing/claude-witness")).unwrap();

        let slug =
            super::slugify_path_segment(&root.join("devpro/ing/claude-witness").to_string_lossy());
        let decoded = super::resolve_slug_under(Path::new("/"), &slug).unwrap();

        assert_eq!(
            std::fs::canonicalize(decoded).unwrap(),
            std::fs::canonicalize(root.join("devpro/ing/claude-witness")).unwrap()
        );
    }

    #[test]
    fn decode_claude_project_slug_recovers_dots_and_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        // '.' also encodes to '-', so "IngTian.github.io" and the ".claude"
        // worktree marker both have to come back exactly.
        let real = root.join("ing/IngTian.github.io/.claude/worktrees/scroll-reveal");
        std::fs::create_dir_all(&real).unwrap();

        let slug = super::slugify_path_segment(&real.to_string_lossy());
        let decoded = super::resolve_slug_under(Path::new("/"), &slug).unwrap();

        assert_eq!(
            std::fs::canonicalize(decoded).unwrap(),
            std::fs::canonicalize(&real).unwrap()
        );
    }

    #[test]
    fn decode_claude_project_slug_ignores_real_paths_and_unknown_dirs() {
        // Already a path: nothing to decode.
        assert_eq!(decode_claude_project_slug("/Users/z/devpro/witness"), None);
        // Slug whose directory does not exist on disk.
        assert_eq!(
            decode_claude_project_slug("-nonexistent-tokscale-probe-dir-xyz"),
            None
        );
    }

    #[test]
    fn deleted_worktree_slugs_still_name_and_group_by_their_repo() {
        // A worktree deleted from disk cannot be resolved, but its slug still
        // carries the marker, so it must not fall back to the raw mangled key.
        let slug = "-Users-zed-devpro-ing-claude-witness--claude-worktrees-store-c1-dissolve";

        assert_eq!(
            workspace_display_label(slug).as_deref(),
            Some("witness ⑃ store-c1-dissolve")
        );
        // And every deleted worktree of that repo shares one rollup identity.
        assert_eq!(
            workspace_repo_root_from_slug(slug).as_deref(),
            Some("-Users-zed-devpro-ing-claude-witness")
        );
        assert_eq!(
            workspace_repo_root_from_slug(
                "-Users-zed-devpro-ing-claude-witness--claude-worktrees-proc-port-43"
            )
            .as_deref(),
            Some("-Users-zed-devpro-ing-claude-witness")
        );
    }

    #[test]
    fn workspace_display_label_falls_back_to_raw_key_when_undecodable() {
        // A deleted project directory cannot be resolved, so the label stays the
        // raw slug rather than becoming empty.
        let slug = "-nonexistent-tokscale-probe-dir-xyz";
        assert_eq!(workspace_display_label(slug).as_deref(), Some(slug));
    }

    #[test]
    fn warp_cache_parser_preserves_requests_and_spend_without_tokens() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{
  "version": 1,
  "syncedAt": "2026-05-29T12:00:00Z",
  "usage": {
    "requestsUsed": 42,
    "requestLimit": 100,
    "spendCents": 1234,
    "nextRefreshTime": "2026-06-01T00:00:00Z"
  },
  "workspaces": [
    {
      "id": "workspace-1",
      "name": "Personal",
      "requestsUsed": 12,
      "spendCents": 345
    }
  ]
}"#,
        )
        .unwrap();

        let messages = crate::sessions::warp::parse_warp_file(file.path());
        assert_eq!(messages.len(), 1);

        let workspace = messages
            .iter()
            .find(|message| message.session_id == "warp-aggregate-workspace-1")
            .unwrap();
        assert_eq!(workspace.client, "warp");
        assert_eq!(workspace.model_id, "aggregate-requests");
        assert_eq!(workspace.provider_id, "warp");
        assert_eq!(workspace.workspace_label.as_deref(), Some("Personal"));
        assert_eq!(workspace.message_count, 12);
        assert_eq!(workspace.tokens, TokenBreakdown::default());
        assert!((workspace.cost - 3.45).abs() < 1e-9);

        std::fs::write(
            file.path(),
            r#"{
  "version": 1,
  "syncedAt": "2026-05-29T12:00:00Z",
  "usage": {
    "requestsUsed": 42,
    "requestLimit": 100,
    "spendCents": 1234,
    "nextRefreshTime": "2026-06-01T00:00:00Z"
  },
  "workspaces": []
}"#,
        )
        .unwrap();

        let messages = crate::sessions::warp::parse_warp_file(file.path());
        assert_eq!(messages.len(), 1);
        let account = &messages[0];
        assert_eq!(account.session_id, "warp-aggregate-account");
        assert_eq!(account.message_count, 42);
        assert_eq!(account.tokens, TokenBreakdown::default());
        assert!((account.cost - 12.34).abs() < 1e-9);
    }

    #[test]
    fn test_timestamp_to_date_with_positive_offset() {
        let kst = FixedOffset::east_opt(9 * 60 * 60).unwrap();
        let ts = 1772512200000_i64; // 2026-03-03T04:30:00Z
        let date = timestamp_to_date_with_timezone(ts, &kst);
        assert_eq!(date, "2026-03-03");
    }

    #[test]
    fn test_timestamp_to_date_with_negative_offset() {
        let pst = FixedOffset::west_opt(8 * 60 * 60).unwrap();
        let ts = 1772512200000_i64; // 2026-03-03T04:30:00Z
        let date = timestamp_to_date_with_timezone(ts, &pst);
        assert_eq!(date, "2026-03-02");
    }

    #[test]
    fn test_timestamp_to_date_invalid_timestamp() {
        let utc = FixedOffset::east_opt(0).unwrap();
        let date = timestamp_to_date_with_timezone(i64::MAX, &utc);
        assert_eq!(date, "");
    }

    #[test]
    fn test_unified_message_creation() {
        let tokens = TokenBreakdown {
            input: 100,
            output: 50,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        };

        let msg = UnifiedMessage::new(
            "opencode",
            "claude-3-5-sonnet",
            "anthropic",
            "test-session-id",
            1733011200000,
            tokens,
            0.05,
        );

        assert_eq!(msg.client, "opencode");
        assert_eq!(msg.model_id, "claude-3-5-sonnet");
        assert_eq!(msg.session_id, "test-session-id");
        assert_eq!(msg.date, timestamp_to_date(1733011200000));
        assert_eq!(msg.cost, 0.05);
        assert_eq!(msg.agent, None);
        assert_eq!(msg.workspace_key, None);
        assert_eq!(msg.workspace_label, None);
    }

    #[test]
    fn test_normalize_workspace_key_normalizes_slashes_and_trailing_separator() {
        assert_eq!(
            normalize_workspace_key(r"C:\Users\alice\repo\"),
            Some("C:/Users/alice/repo".to_string())
        );
        assert_eq!(
            normalize_workspace_key("/Users/alice//repo/"),
            Some("/Users/alice/repo".to_string())
        );
    }

    #[test]
    fn test_normalize_workspace_key_preserves_unc_prefix() {
        assert_eq!(
            normalize_workspace_key(r"\\server\share\repo\"),
            Some("//server/share/repo".to_string())
        );
        assert_eq!(
            normalize_workspace_key("//server//share///repo/"),
            Some("//server/share/repo".to_string())
        );
    }

    #[test]
    fn test_workspace_label_from_key_uses_last_path_segment() {
        assert_eq!(
            workspace_label_from_key("/Users/alice/my-repo"),
            Some("my-repo".to_string())
        );
        assert_eq!(
            workspace_label_from_key("encoded-project-key"),
            Some("encoded-project-key".to_string())
        );
    }

    #[test]
    fn test_normalize_agent_name() {
        assert_eq!(normalize_agent_name("OmO"), "Sisyphus");
        assert_eq!(normalize_agent_name("Sisyphus"), "Sisyphus");
        assert_eq!(normalize_agent_name("omo"), "Sisyphus");
        assert_eq!(normalize_agent_name("sisyphus"), "Sisyphus");
        assert_eq!(
            normalize_agent_name("Sisyphus (Ultraworker)"),
            "Sisyphus (Ultraworker)"
        );

        assert_eq!(
            normalize_opencode_agent_name("Sisyphus (Ultraworker)"),
            "Sisyphus"
        );
        assert_eq!(normalize_opencode_agent_name("hephaestus"), "Hephaestus");
        assert_eq!(normalize_opencode_agent_name("prometheus"), "Prometheus");
        assert_eq!(normalize_opencode_agent_name("atlas"), "Atlas");
        assert_eq!(normalize_opencode_agent_name("metis"), "Metis");
        assert_eq!(normalize_opencode_agent_name("momus"), "Momus");
        assert_eq!(
            normalize_opencode_agent_name("sisyphus-junior"),
            "Sisyphus-Junior"
        );
        assert_eq!(
            normalize_opencode_agent_name("planner-sisyphus"),
            "Planner-Sisyphus"
        );

        assert_eq!(
            normalize_opencode_agent_name("Hephaestus (Deep Agent)"),
            "Hephaestus"
        );
        assert_eq!(
            normalize_opencode_agent_name("Prometheus (Plan Builder)"),
            "Prometheus"
        );
        assert_eq!(
            normalize_opencode_agent_name("Prometheus (Planner)"),
            "Prometheus"
        );
        assert_eq!(
            normalize_opencode_agent_name("Atlas (Plan Executor)"),
            "Atlas"
        );
        assert_eq!(
            normalize_opencode_agent_name("Metis (Plan Consultant)"),
            "Metis"
        );
        assert_eq!(
            normalize_opencode_agent_name("Momus (Plan Critic)"),
            "Momus"
        );
        assert_eq!(
            normalize_opencode_agent_name("Momus (Plan Reviewer)"),
            "Momus"
        );

        assert_eq!(normalize_agent_name("OmO-Plan"), "Planner-Sisyphus");
        assert_eq!(normalize_agent_name("Planner-Sisyphus"), "Planner-Sisyphus");
        assert_eq!(normalize_agent_name("omo-plan"), "Planner-Sisyphus");

        assert_eq!(normalize_agent_name("orchestrator-sisyphus"), "Atlas");
        assert_eq!(
            normalize_opencode_agent_name("orchestrator-sisyphus"),
            "Atlas"
        );
        assert_eq!(normalize_agent_name("explore"), "Explore");
        assert_eq!(normalize_agent_name("CustomAgent"), "CustomAgent");

        assert_eq!(normalize_agent_name("executor"), "Executor");
        assert_eq!(
            normalize_agent_name("task-orchestrator"),
            "Task Orchestrator"
        );
        assert_eq!(normalize_agent_name("git-committer"), "Git Committer");
        assert_eq!(
            normalize_agent_name("frontend-ui-ux-engineer"),
            "Frontend UI UX Engineer"
        );
        assert_eq!(
            normalize_agent_name("astrape:executor-high"),
            "Executor High"
        );
        assert_eq!(
            normalize_agent_name("oh-my-claudecode:code-reviewer"),
            "Code Reviewer"
        );
    }

    #[test]
    fn test_normalize_copilot_agent_name() {
        assert_eq!(
            normalize_copilot_agent_name("github.copilot.default"),
            "GitHub Copilot"
        );
        assert_eq!(
            normalize_copilot_agent_name("GITHUB.COPILOT.DEFAULT"),
            "GitHub Copilot"
        );
        assert_eq!(normalize_copilot_agent_name("github.copilot.chat"), "Chat");
        assert_eq!(
            normalize_copilot_agent_name("Plugin:software-engineering-team:se-ux-ui-designer"),
            "Software Engineering Team: Se UX UI Designer"
        );
        assert_eq!(
            normalize_copilot_agent_name("plugin:my-team:my-agent"),
            "My Team: My Agent"
        );
        assert_eq!(
            normalize_copilot_agent_name("Plugin:code-review-team:api-reviewer"),
            "Code Review Team: API Reviewer"
        );
        assert_eq!(
            normalize_copilot_agent_name("some-custom-agent"),
            "Some Custom Agent"
        );
        assert_eq!(normalize_agent_name("oh-my-codex:librarian"), "Librarian");
        assert_eq!(normalize_agent_name("astrape:executor"), "Executor");
        assert_eq!(normalize_agent_name("plan-reviewer"), "Plan Reviewer");
        assert_eq!(normalize_agent_name("astrape:planner"), "Planner");

        assert_eq!(
            normalize_opencode_agent_name("astrape:sisyphus"),
            "Sisyphus"
        );
        assert_eq!(
            normalize_opencode_agent_name("oh-my-claudecode:executor"),
            "Executor"
        );

        // New dash format (oh-my-openagent current)
        assert_eq!(
            normalize_opencode_agent_name("Sisyphus - Ultraworker"),
            "Sisyphus"
        );
        assert_eq!(
            normalize_opencode_agent_name("Hephaestus - Deep Agent"),
            "Hephaestus"
        );
        assert_eq!(
            normalize_opencode_agent_name("Prometheus - Plan Builder"),
            "Prometheus"
        );
        assert_eq!(
            normalize_opencode_agent_name("Atlas - Plan Executor"),
            "Atlas"
        );
        assert_eq!(
            normalize_opencode_agent_name("Metis - Plan Consultant"),
            "Metis"
        );
        assert_eq!(
            normalize_opencode_agent_name("Momus - Plan Critic"),
            "Momus"
        );

        // ZWSP-prefixed names (oh-my-openagent sort-order prefixes)
        assert_eq!(
            normalize_opencode_agent_name("\u{200B}Sisyphus - Ultraworker"),
            "Sisyphus"
        );
        assert_eq!(
            normalize_opencode_agent_name("\u{200B}\u{200B}\u{200B}Prometheus - Plan Builder"),
            "Prometheus"
        );
        assert_eq!(
            normalize_opencode_agent_name("\u{200B}\u{200B}\u{200B}\u{200B}Atlas - Plan Executor"),
            "Atlas"
        );
        assert_eq!(
            normalize_opencode_agent_name("\u{FEFF}Momus - Plan Critic"),
            "Momus"
        );
        assert_eq!(
            normalize_opencode_agent_name("\u{200B}sisyphus-junior"),
            "Sisyphus-Junior"
        );
        assert_eq!(
            normalize_opencode_agent_name("\u{200B}sisyphus"),
            "Sisyphus"
        );
        assert_eq!(
            normalize_opencode_agent_name("\u{200B}  Sisyphus   -   Ultraworker  "),
            "Sisyphus"
        );
        assert_eq!(
            normalize_opencode_agent_name("\u{200B}\u{200B}\u{200B}   Prometheus    Plan Builder"),
            "Prometheus"
        );
    }

    #[test]
    fn test_strip_zero_width_chars() {
        assert_eq!(strip_zero_width_chars("hello"), "hello");
        assert_eq!(strip_zero_width_chars("\u{200B}hello"), "hello");
        assert_eq!(
            strip_zero_width_chars("\u{200B}\u{200B}\u{200B}hello"),
            "hello"
        );
        assert_eq!(strip_zero_width_chars("\u{FEFF}hello"), "hello");
        assert_eq!(strip_zero_width_chars("\u{200C}hello\u{200D}"), "hello");
        assert_eq!(strip_zero_width_chars(""), "");
        assert_eq!(
            strip_zero_width_chars("no special chars"),
            "no special chars"
        );
    }
}
