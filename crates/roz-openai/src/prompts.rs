//! Codex system prompts snapshot.
//!
//! Pinned to openai/codex SHA `da86cedbd439d38fbd7e613e4e88f8f6f138debb`.
//!
//! # Re-sync procedure
//!
//! 1. Pick a new upstream SHA from <https://github.com/openai/codex>.
//! 2. Refresh each file under `../prompts/` via:
//!    ```sh
//!    gh api repos/openai/codex/contents/codex-rs/core/<upstream_name>?ref=<sha> \
//!        --jq '.content' | base64 -d > crates/roz-openai/prompts/<local_name>
//!    ```
//!    (Re-prepend the 3-line provenance HTML comment.)
//! 3. Update the SHA reference in this module doc and in `NOTICE`.
//! 4. Run `cargo test -p roz-openai prompts::` to confirm `include_str!` still resolves.

/// gpt-5.1 default system prompt.
pub const GPT_5_1_PROMPT: &str = include_str!("../prompts/gpt_5_1_prompt.md");
/// gpt-5.2 default system prompt.
pub const GPT_5_2_PROMPT: &str = include_str!("../prompts/gpt_5_2_prompt.md");
/// gpt-5 codex-tuned system prompt (covers gpt-5.1-codex, -codex-mini, and unknown codex variants).
pub const GPT_5_CODEX_PROMPT: &str = include_str!("../prompts/gpt_5_codex_prompt.md");
/// gpt-5.1-codex-max system prompt.
pub const GPT_5_1_CODEX_MAX_PROMPT: &str = include_str!("../prompts/gpt_5_1_codex_max_prompt.md");
/// gpt-5.2-codex system prompt.
pub const GPT_5_2_CODEX_PROMPT: &str = include_str!("../prompts/gpt_5_2_codex_prompt.md");

/// Map a model family/name to its Codex system prompt.
///
/// Exact matches are checked first for the codex-max and gpt-5.2-codex families.
/// Any other string containing `"codex"` falls back to the shared `GPT_5_CODEX_PROMPT`.
/// `"gpt-5.2"` maps to `GPT_5_2_PROMPT`.
/// Everything else (including unknown families and empty string) falls back to
/// `GPT_5_1_PROMPT`, matching Codex CLI default behavior.
#[must_use]
pub fn codex_instructions(model_family: &str) -> &'static str {
    match model_family {
        "gpt-5.2-codex" => GPT_5_2_CODEX_PROMPT,
        "gpt-5.1-codex-max" => GPT_5_1_CODEX_MAX_PROMPT,
        "gpt-5.2" => GPT_5_2_PROMPT,
        s if s.contains("codex") => GPT_5_CODEX_PROMPT,
        _ => GPT_5_1_PROMPT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_prompt_const_nonempty() {
        assert!(!GPT_5_1_PROMPT.is_empty(), "GPT_5_1_PROMPT empty");
        assert!(!GPT_5_2_PROMPT.is_empty(), "GPT_5_2_PROMPT empty");
        assert!(!GPT_5_CODEX_PROMPT.is_empty(), "GPT_5_CODEX_PROMPT empty");
        assert!(!GPT_5_1_CODEX_MAX_PROMPT.is_empty(), "GPT_5_1_CODEX_MAX_PROMPT empty");
        assert!(!GPT_5_2_CODEX_PROMPT.is_empty(), "GPT_5_2_CODEX_PROMPT empty");
    }

    #[test]
    fn codex_instructions_dispatches_gpt_5_2_codex() {
        assert_eq!(codex_instructions("gpt-5.2-codex"), GPT_5_2_CODEX_PROMPT);
    }

    #[test]
    fn codex_instructions_dispatches_gpt_5_1_codex_max() {
        assert_eq!(codex_instructions("gpt-5.1-codex-max"), GPT_5_1_CODEX_MAX_PROMPT);
    }

    #[test]
    fn codex_instructions_dispatches_gpt_5_codex_for_plain_codex_name() {
        assert_eq!(codex_instructions("gpt-5.1-codex"), GPT_5_CODEX_PROMPT);
        assert_eq!(codex_instructions("gpt-5.1-codex-mini"), GPT_5_CODEX_PROMPT);
        // Catch-all for any string containing "codex" that did not match a more-specific arm.
        assert_eq!(codex_instructions("some-future-codex-variant"), GPT_5_CODEX_PROMPT);
    }

    #[test]
    fn codex_instructions_dispatches_gpt_5_2() {
        assert_eq!(codex_instructions("gpt-5.2"), GPT_5_2_PROMPT);
    }

    #[test]
    fn codex_instructions_falls_back_to_gpt_5_1() {
        assert_eq!(codex_instructions("unknown-model"), GPT_5_1_PROMPT);
        assert_eq!(codex_instructions("gpt-5.1"), GPT_5_1_PROMPT);
        assert_eq!(codex_instructions(""), GPT_5_1_PROMPT);
    }
}
