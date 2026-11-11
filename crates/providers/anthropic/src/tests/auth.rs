//! Unit tests: auth.

use crate::auth::{OAUTH_SYSTEM_PROMPT, oauth_billing_header, sanitize_oauth_text};
use crate::tests::support::test_anthropic;
use crate::wire::system::{AnthropicSystemPrompt, CacheControlEphemeral, TextBlockParam};

/// The default qmtcode system prompt, shared with the agent crate's
/// embedded profiles (see `crates/agent/examples/confs/single_coder.toml`).
const DEFAULT_QMTCODE_SYSTEM_PROMPT: &str =
    include_str!("../../../../agent/examples/prompts/default_system.txt");

#[test]
fn billing_header_matches_opencode_fixture() {
    assert_eq!(
        oauth_billing_header("hello world test message", "2.1.87"),
        "x-anthropic-billing-header: cc_version=2.1.87.6ff; cc_entrypoint=sdk-cli; cch=4ffc3;"
    );
}

#[test]
fn test_sanitize_system_prompt_oauth_no_system() {
    // OAuth with no system prompt → OAUTH_SYSTEM_PROMPT injected as the sole block
    let anthropic = test_anthropic("sk-ant-oat01-abc123");
    let sanitized = anthropic.sanitize_system_prompt();
    match sanitized {
        Some(AnthropicSystemPrompt::Blocks(blocks)) => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
            assert!(blocks[0].cache_control.is_none());
        }
        other => panic!("Expected Blocks with one OAuth block, got {:?}", other),
    }
}

#[test]
fn test_sanitize_system_prompt_oauth_text() {
    // OAuth with a plain Text prompt → OAUTH_SYSTEM_PROMPT prepended, original preserved
    let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
    anthropic.system = Some(AnthropicSystemPrompt::Text(
        "You are QueryMT assistant.".to_string(),
    ));
    let sanitized = anthropic.sanitize_system_prompt();
    match sanitized {
        Some(AnthropicSystemPrompt::Blocks(blocks)) => {
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
            assert_eq!(blocks[1].text, "You are QueryMT assistant.");
        }
        other => panic!("Expected Blocks with two entries, got {:?}", other),
    }
}

#[test]
fn test_sanitize_system_prompt_oauth_blocks() {
    // OAuth with existing Blocks → OAUTH_SYSTEM_PROMPT prepended, original blocks preserved
    let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
    anthropic.system = Some(AnthropicSystemPrompt::Blocks(vec![TextBlockParam {
        block_type: "text".to_string(),
        text: "You are QueryMT assistant.".to_string(),
        cache_control: Some(CacheControlEphemeral {
            control_type: "ephemeral".to_string(),
            ttl: None,
        }),
        citations: None,
    }]));
    let sanitized = anthropic.sanitize_system_prompt();
    match sanitized {
        Some(AnthropicSystemPrompt::Blocks(blocks)) => {
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
            assert!(blocks[0].cache_control.is_none());
            assert_eq!(blocks[1].text, "You are QueryMT assistant.");
            // cache_control on the original block should be preserved
            assert!(blocks[1].cache_control.is_some());
        }
        other => panic!("Expected Blocks with two entries, got {:?}", other),
    }
}

#[test]
fn test_sanitize_system_prompt_non_oauth_passthrough() {
    // Non-OAuth: system prompt returned unchanged, no blocks injected
    let mut anthropic = test_anthropic("sk-ant-api03-xyz789");
    anthropic.system = Some(AnthropicSystemPrompt::Text(
        "You are QueryMT assistant.".to_string(),
    ));
    let sanitized = anthropic.sanitize_system_prompt();
    assert_eq!(
        sanitized,
        Some(AnthropicSystemPrompt::Text(
            "You are QueryMT assistant.".to_string()
        ))
    );
}

#[test]
fn sanitize_oauth_text_prunes_identity_paragraph() {
    // Identity in its own paragraph is removed along with the paragraph.
    let text = "You are QueryMT, a coding agent for software engineering tasks.\n\nUse the available tools to help the user accurately and efficiently.";
    assert_eq!(
        sanitize_oauth_text(text),
        "Use the available tools to help the user accurately and efficiently."
    );
}

#[test]
fn sanitize_oauth_text_prunes_identity_line_within_paragraph() {
    // Only the identity line is removed; sibling lines in the same
    // paragraph survive.
    let text = "You are QueryMT, a coding agent for software engineering tasks.\nUse the available tools to help the user accurately and efficiently.";
    assert_eq!(
        sanitize_oauth_text(text),
        "Use the available tools to help the user accurately and efficiently."
    );
}

#[test]
fn sanitize_oauth_text_keeps_env_metadata_next_to_powered_by() {
    // code_meta.jinja2 renders the identity line and `<env>` metadata in
    // one paragraph; only the identity line must be pruned.
    let text = "You are powered by anthropic/claude-sonnet-4-5-20250929.\n<env>\n  Is directory a git repo: yes\n  Platform: macos\n</env>";
    let sanitized = sanitize_oauth_text(text);
    assert!(!sanitized.contains("You are powered by"));
    assert!(sanitized.contains("<env>"));
    assert!(sanitized.contains("Platform: macos"));
}

#[test]
fn sanitize_oauth_text_preserves_default_qmtcode_prompt_instructions() {
    let sanitized = sanitize_oauth_text(DEFAULT_QMTCODE_SYSTEM_PROMPT);
    assert!(!sanitized.contains("You are QueryMT,"));
    // Operational instructions and QueryMT references in guidance stay.
    assert!(sanitized.contains("# Task management"));
    assert!(sanitized.contains("# Code search policy"));
    assert!(sanitized.contains("# Tool usage policy"));
    assert!(sanitized.contains("answer from QueryMT docs"));
}

#[test]
fn sanitize_system_prompt_oauth_prunes_identity_only_text_prompt() {
    // A prompt consisting solely of the identity line prunes to nothing;
    // only the injected OAuth identity block remains.
    let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
    anthropic.system = Some(AnthropicSystemPrompt::Text(
        "You are QueryMT, a coding agent for software engineering tasks.".to_string(),
    ));
    let sanitized = anthropic.sanitize_system_prompt();
    match sanitized {
        Some(AnthropicSystemPrompt::Blocks(blocks)) => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
        }
        other => panic!("Expected single OAuth identity block, got {:?}", other),
    }
}

#[test]
fn sanitize_system_prompt_oauth_drops_identity_only_blocks() {
    // Identity-only blocks are dropped; instruction blocks (with their
    // cache_control) are preserved alongside the injected identity block.
    let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
    anthropic.system = Some(AnthropicSystemPrompt::Blocks(vec![
        TextBlockParam {
            block_type: "text".to_string(),
            text: "You are QueryMT, a coding agent for software engineering tasks.".to_string(),
            cache_control: None,
            citations: None,
        },
        TextBlockParam {
            block_type: "text".to_string(),
            text: "# Code search policy\n- Use `search_text` first for content search.".to_string(),
            cache_control: Some(CacheControlEphemeral {
                control_type: "ephemeral".to_string(),
                ttl: None,
            }),
            citations: None,
        },
    ]));
    let sanitized = anthropic.sanitize_system_prompt();
    match sanitized {
        Some(AnthropicSystemPrompt::Blocks(blocks)) => {
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
            assert!(blocks[1].text.starts_with("# Code search policy"));
            assert!(blocks[1].cache_control.is_some());
        }
        other => panic!("Expected Blocks with two entries, got {:?}", other),
    }
}
