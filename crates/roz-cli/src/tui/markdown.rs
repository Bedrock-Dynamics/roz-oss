use owo_colors::OwoColorize;
use regex::Regex;
use std::sync::LazyLock;

static BOLD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
static CODE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());

/// Stateful markdown-to-ANSI renderer that processes one line at a time.
pub struct MarkdownRenderer {
    in_code_block: bool,
}

impl Default for MarkdownRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownRenderer {
    pub const fn new() -> Self {
        Self { in_code_block: false }
    }

    /// Process a single line of markdown, returning ANSI-styled text.
    pub fn render_line(&mut self, line: &str) -> String {
        // Code block fence
        if line.starts_with("```") {
            self.in_code_block = !self.in_code_block;
            return format!("  {}", "\u{2500}".repeat(40).dimmed());
        }

        // Inside code block — dim, indented
        if self.in_code_block {
            return format!("  {}", line.dimmed());
        }

        render_inline(line)
    }
}

/// Render inline markdown (headers, bold, code) for a single line.
fn render_inline(line: &str) -> String {
    // Headers
    if let Some(text) = line.strip_prefix("### ") {
        return format!("{}", text.yellow().bold());
    }
    if let Some(text) = line.strip_prefix("## ") {
        return format!("{}", text.yellow().bold());
    }
    if let Some(text) = line.strip_prefix("# ") {
        return format!("{}", text.yellow().bold());
    }

    let mut out = line.to_string();

    // Bold: **text** → bold
    out = BOLD_RE
        .replace_all(&out, |caps: &regex::Captures| format!("{}", caps[1].to_string().bold()))
        .to_string();

    // Inline code: `text` → cyan
    out = CODE_RE
        .replace_all(&out, |caps: &regex::Captures| format!("{}", caps[1].to_string().cyan()))
        .to_string();

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_detected() {
        let mut r = MarkdownRenderer::new();
        let out = r.render_line("# Hello");
        assert!(out.contains("Hello"));
    }

    #[test]
    fn code_block_toggle() {
        let mut r = MarkdownRenderer::new();
        assert!(!r.in_code_block);
        let _ = r.render_line("```rust");
        assert!(r.in_code_block);
        let _ = r.render_line("```");
        assert!(!r.in_code_block);
    }

    #[test]
    fn plain_text_unchanged() {
        let mut r = MarkdownRenderer::new();
        let out = r.render_line("just regular text");
        assert_eq!(out, "just regular text");
    }
}
