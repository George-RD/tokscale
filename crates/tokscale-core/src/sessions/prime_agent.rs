//! Prime Agent session parser.
//!
//! Prime Agent stores root sessions in `~/.prime/agent/sessions/*.jsonl` and
//! RLM child sessions below the sibling `session-artifacts` tree. Both use the
//! Pi append-only JSONL record format, so token extraction is shared with the
//! Pi parser. `child_usage_attributed` records are intentionally ignored: they
//! are accounting metadata that folds a child's usage into a parent message at
//! runtime, while tokscale scans the child's own session file directly.

use super::pi::parse_pi_format_rlm_file;
use super::UnifiedMessage;
use std::path::Path;

pub fn parse_prime_agent_file(path: &Path) -> Vec<UnifiedMessage> {
    parse_pi_format_rlm_file(path, "prime-agent", "prime-agent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn session_file(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn parses_root_session_without_counting_child_attribution_records() {
        let file = session_file(
            r#"{"type":"session","version":3,"id":"root-1","timestamp":"2026-08-08T00:00:00.000Z","cwd":"/tmp/project","rlmDepth":0}
{"type":"session_info","id":"info","parentId":null,"timestamp":"2026-08-08T00:00:00.500Z","name":"My renamed thread"}
{"type":"message","id":"assistant-1","parentId":"info","timestamp":"2026-08-08T00:00:01.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-5","responseId":"msg_provider_001","usage":{"input":100,"output":50,"cacheRead":20,"cacheWrite":10,"totalTokens":180}}}
{"type":"child_usage_attributed","id":"usage-1","parentId":"assistant-1","timestamp":"2026-08-08T00:00:02.000Z","targetId":"assistant-1","childUsage":{"input":500,"output":200,"cacheRead":0,"cacheWrite":0,"totalTokens":700},"aggregateUsage":{"input":600,"output":250,"cacheRead":20,"cacheWrite":10,"totalTokens":880},"origin":"spawn_task"}"#,
        );

        let messages = parse_prime_agent_file(file.path());

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.client, "prime-agent");
        assert_eq!(message.session_id, "root-1");
        assert_eq!(message.workspace_key.as_deref(), Some("/tmp/project"));
        assert_eq!(message.tokens.input, 100);
        assert_eq!(message.tokens.output, 50);
        assert_eq!(message.tokens.cache_read, 20);
        assert_eq!(message.tokens.cache_write, 10);
        assert_eq!(message.agent, None, "a root thread name is not an agent");
        assert_eq!(
            message.dedup_key.as_deref(),
            Some("prime-agent:response:msg_provider_001")
        );
    }

    #[test]
    fn attributes_rlm_child_messages_to_the_session_name() {
        let file = session_file(
            r#"{"type":"session","version":3,"id":"child-1","timestamp":"2026-08-08T00:00:00.000Z","cwd":"/tmp/project","parentSession":"/tmp/root.jsonl","rlmDepth":1}
{"type":"session_info","id":"info","parentId":null,"timestamp":"2026-08-08T00:00:00.500Z","name":"api-reviewer"}
{"type":"message","id":"assistant-1","parentId":"info","timestamp":"2026-08-08T00:00:01.000Z","message":{"role":"assistant","provider":"openai","model":"gpt-5.4","usage":{"input":40,"output":12,"cacheRead":8,"cacheWrite":0,"totalTokens":60}}}"#,
        );

        let messages = parse_prime_agent_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].agent.as_deref(), Some("api-reviewer"));
        assert_eq!(messages[0].provider_id, "openai");
        assert_eq!(messages[0].model_id, "gpt-5.4");
    }

    #[test]
    fn copied_fork_history_keeps_a_cross_session_dedup_key() {
        let original = session_file(
            r#"{"type":"session","version":3,"id":"root-1","timestamp":"2026-08-08T00:00:00.000Z","cwd":"/tmp/project","rlmDepth":0}
{"type":"message","id":"assistant-1","parentId":null,"timestamp":"2026-08-08T00:00:01.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-5","responseId":"msg_provider_001","usage":{"input":100,"output":50,"cacheRead":20,"cacheWrite":10,"totalTokens":180}}}"#,
        );
        let fork = session_file(
            r#"{"type":"session","version":3,"id":"fork-2","timestamp":"2026-08-08T01:00:00.000Z","cwd":"/tmp/project","parentSession":"/tmp/root.jsonl","rlmDepth":0}
{"type":"message","id":"assistant-1","parentId":null,"timestamp":"2026-08-08T00:00:01.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-5","responseId":"msg_provider_001","usage":{"input":100,"output":50,"cacheRead":20,"cacheWrite":10,"totalTokens":180}}}"#,
        );

        let original = parse_prime_agent_file(original.path());
        let fork = parse_prime_agent_file(fork.path());

        assert_eq!(original.len(), 1);
        assert_eq!(fork.len(), 1);
        assert_eq!(original[0].dedup_key, fork[0].dedup_key);
    }

    #[test]
    fn rejects_the_rlm_subagent_catalog_as_a_session() {
        let file = session_file(
            r#"{"type":"rlm_subagent","childId":"sub-deadbeef","sessionName":"worker","sessionFile":"/tmp/child.jsonl"}"#,
        );

        assert!(parse_prime_agent_file(file.path()).is_empty());
    }
}
