//! Capability-limited Mermaid rendering for the terminal transcript.
//!
//! The upstream uses `grok-mermaid`, which is not part of the Rust-only
//! dependency graph. This module implements the useful, deterministic
//! terminal subset directly: left-to-right flowcharts with rectangular nodes
//! and directed links. Unsupported Mermaid syntax is retained as source; a
//! parser warning is shown only when the supported renderer produced a
//! partial diagram, matching the upstream transform's recoverable fallback.

use super::tui_theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MermaidRender {
    Rendered(String),
    /// The renderer can produce a useful partial diagram, but the parser also
    /// reported recoverable warnings. Upstream displays the source plus the
    /// first warning for a completed message, and the partial art while a
    /// message is streaming.
    RenderedWithWarnings {
        rendered: String,
        warnings: Vec<String>,
    },
    Unsupported(String),
}

pub fn renderer_available() -> bool {
    true
}

fn code_fence_parts(markdown: &str) -> Vec<(usize, usize, String, String)> {
    let lines: Vec<&str> = markdown.split('\n').collect();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("```") else {
            index += 1;
            continue;
        };
        let language = rest.trim().to_ascii_lowercase();
        let start = index;
        index += 1;
        let content_start = index;
        while index < lines.len() && !lines[index].trim().starts_with("```") {
            index += 1;
        }
        if index < lines.len() {
            blocks.push((
                start,
                index + 1,
                language,
                lines[content_start..index].join("\n"),
            ));
            index += 1;
        } else {
            // Marked exposes an unfinished code token while a response is
            // streaming. Keeping the partial body here lets the context-aware
            // transformer render a useful incomplete flowchart.
            blocks.push((start, index, language, lines[content_start..].join("\n")));
        }
    }
    blocks
}

fn node_label(token: &str, allow_class_suffix: bool) -> Option<(String, String)> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let token = if let Some((base, _class_name)) = token.split_once(":::") {
        if !allow_class_suffix {
            return None;
        }
        base.trim()
    } else {
        token
    };
    if token.is_empty() {
        return None;
    }
    if let Some(open) = token.find('[') {
        let close = token.rfind(']')?;
        if close <= open || close + 1 != token.len() {
            return None;
        }
        let id = token[..open].trim();
        let label = token[open + 1..close].trim();
        if id.is_empty() || label.is_empty() {
            return None;
        }
        return Some((id.to_string(), label.to_string()));
    }
    let id = token.split_whitespace().next()?;
    if id.len() != token.len() {
        return None;
    }
    Some((id.to_string(), id.to_string()))
}

fn class_warning(left: &str, right: &str) -> Option<String> {
    let class_start = left.find(":::")?;
    Some(format!(
        "dropped, expected a link: \"{} --> {}\"",
        &left[class_start..],
        right
    ))
}

fn styled_box_line(line: String, styled: bool) -> String {
    if styled {
        tui_theme::fg("borderMuted", line)
    } else {
        line
    }
}

fn styled_middle_line(left_label: &str, right_label: &str, styled: bool) -> String {
    if !styled {
        return format!("│ {left_label} ├───▶│ {right_label} │");
    }

    format!(
        "{}{}{}{}{}{}{}",
        tui_theme::fg("borderMuted", "│"),
        tui_theme::fg("text", format!(" {left_label} ")),
        tui_theme::fg("borderMuted", "├"),
        tui_theme::fg("accent", "───▶"),
        tui_theme::fg("borderMuted", "│"),
        tui_theme::fg("text", format!(" {right_label} ")),
        tui_theme::fg("borderMuted", "│"),
    )
}

fn flowchart_lines(source: &str, allow_class_suffix: bool, styled: bool) -> MermaidRender {
    let mut lines = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let Some(direction_line) = lines.next() else {
        return MermaidRender::Unsupported("empty Mermaid diagram".to_string());
    };
    let mut direction_parts = direction_line.split_whitespace();
    let Some(kind) = direction_parts.next() else {
        return MermaidRender::Unsupported("empty Mermaid diagram".to_string());
    };
    if !kind.eq_ignore_ascii_case("flowchart") && !kind.eq_ignore_ascii_case("graph") {
        return MermaidRender::Unsupported(format!("unsupported diagram type: {direction_line}"));
    }
    let direction = direction_parts.next().unwrap_or_default();
    if !direction.eq_ignore_ascii_case("LR") {
        return MermaidRender::Unsupported(
            "only left-to-right flowcharts are supported".to_string(),
        );
    }

    let mut edges = Vec::new();
    let mut warnings = Vec::new();
    for line in lines {
        let Some((left, right)) = line.split_once("-->") else {
            return MermaidRender::Unsupported(format!("unsupported flowchart syntax: {line}"));
        };
        let left = left.trim();
        let right = right.trim();
        if !allow_class_suffix {
            if let Some(warning) = class_warning(left, right) {
                warnings.push(warning);
            } else if right.contains(":::") {
                warnings.push(format!(
                    "dropped, expected a link: \"{} --> {}\"",
                    left, right
                ));
            }
        }
        let Some((left_id, left_label)) = node_label(left, true) else {
            return MermaidRender::Unsupported(format!("invalid node: {left}"));
        };
        let Some((right_id, right_label)) = node_label(right, true) else {
            return MermaidRender::Unsupported(format!("invalid node: {right}"));
        };
        edges.push((left_id, left_label, right_id, right_label));
    }
    if edges.is_empty() {
        return MermaidRender::Unsupported("flowchart has no directed links".to_string());
    }

    let mut rows = Vec::new();
    for (left_id, left_label, right_id, right_label) in edges {
        let _ = (left_id, right_id);
        let left_width = left_label.chars().count() + 2;
        let right_width = right_label.chars().count() + 2;
        let gap = 4usize;
        let top = format!(
            "┌{}┐{}┌{}┐",
            "─".repeat(left_width),
            " ".repeat(gap),
            "─".repeat(right_width)
        );
        let middle = styled_middle_line(&left_label, &right_label, styled);
        let bottom = format!(
            "└{}┘{}└{}┘",
            "─".repeat(left_width),
            " ".repeat(gap),
            "─".repeat(right_width)
        );
        rows.push(styled_box_line(top, styled));
        rows.push(middle);
        rows.push(styled_box_line(bottom, styled));
    }
    let rendered = rows.join("\n");
    if warnings.is_empty() {
        MermaidRender::Rendered(rendered)
    } else {
        MermaidRender::RenderedWithWarnings { rendered, warnings }
    }
}

/// Render a completed Mermaid subset without applying Markdown context.
pub fn render_mermaid(source: &str) -> MermaidRender {
    flowchart_lines(source, false, false)
}

fn render_mermaid_for_context(source: &str, is_streaming: bool) -> MermaidRender {
    // Styling is applied after the diagram has been parsed, like upstream's
    // `themedLines`. `tui_theme::fg` is a no-op until a theme is loaded, so
    // callers that render plain output retain the same text contract.
    flowchart_lines(source, is_streaming, true)
}

fn longest_backtick_run(content: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in content.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn code_span(line: &str) -> String {
    // Encode each diagram row as an inline code span so Markdown preserves its
    // spacing and box-drawing characters. A non-breaking space keeps an empty
    // row visible, and a longer delimiter preserves literal backticks.
    let content = if line.is_empty() { "\u{00a0}" } else { line };
    let fence = "`".repeat(longest_backtick_run(content) + 1);
    let padding = if content.starts_with('`') || content.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{padding}{content}{padding}{fence}")
}

fn warning_text(warnings: &[String]) -> String {
    let Some(first) = warnings.first() else {
        return String::new();
    };
    if warnings.len() == 1 {
        first.clone()
    } else {
        format!("{first} (+{} more)", warnings.len() - 1)
    }
}

fn warning_block(source: &str, warnings: &[String]) -> String {
    let warning = format!("Mermaid diagram not rendered: {}", warning_text(warnings));
    format!(
        "{source}\n{}  ",
        code_span(&tui_theme::fg("warning", warning))
    )
}

/// Transform Mermaid fences with explicit upstream Markdown context.
///
/// `message_type` is normally `assistant` or `assistant-thinking`. The older
/// [`transform_markdown`] wrapper intentionally keeps its final-message
/// behavior; interactive mode must use this additive seam once its Markdown
/// callback can forward `is_streaming` and the message type.
pub fn transform_markdown_with_context(
    markdown: &str,
    width: usize,
    mode: &str,
    is_streaming: bool,
    message_type: &str,
) -> String {
    if mode == "off"
        || !renderer_available()
        || message_type.eq_ignore_ascii_case("assistant-thinking")
        || (is_streaming && mode != "streaming")
    {
        return markdown.to_string();
    }
    let lines: Vec<&str> = markdown.split('\n').collect();
    let blocks = code_fence_parts(markdown);
    if blocks.is_empty() {
        return markdown.to_string();
    }
    let mut result = String::new();
    let mut cursor = 0usize;
    for (start, end, language, source) in blocks {
        if start < cursor {
            continue;
        }
        result.push_str(&lines[cursor..start].join("\n"));
        if cursor != start {
            result.push('\n');
        }
        if language.split_whitespace().next() != Some("mermaid") {
            result.push_str(&lines[start..end].join("\n"));
        } else {
            let source_block = lines[start..end].join("\n");
            match render_mermaid_for_context(&source, is_streaming) {
                MermaidRender::Rendered(rendered) => {
                    if rendered
                        .lines()
                        .any(|line| pi_tui::utils::visible_width(line) > width)
                    {
                        result.push_str(&source_block);
                    } else {
                        result.push_str(
                            &rendered
                                .lines()
                                .map(code_span)
                                .collect::<Vec<_>>()
                                .join("  \n"),
                        );
                    }
                }
                MermaidRender::RenderedWithWarnings { rendered, warnings } => {
                    let too_wide = rendered
                        .lines()
                        .any(|line| pi_tui::utils::visible_width(line) > width);
                    if too_wide {
                        result.push_str(&source_block);
                    } else if is_streaming {
                        result.push_str(
                            &rendered
                                .lines()
                                .map(code_span)
                                .collect::<Vec<_>>()
                                .join("  \n"),
                        );
                    } else {
                        result.push_str(&warning_block(&source_block, &warnings));
                    }
                }
                // An unsupported diagram has no trustworthy terminal art.
                // Preserve the exact source so users can still copy or read
                // it; upstream does not add a warning when no Mermaid art was
                // produced at all.
                MermaidRender::Unsupported(_) => result.push_str(&source_block),
            }
        }
        cursor = end;
    }
    if cursor < lines.len() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&lines[cursor..].join("\n"));
    }
    result
}

/// Backward-compatible transform for the existing two-argument Markdown
/// callback. It has no stream/message context, so it renders as a completed
/// assistant message; the parent mode can opt into the additive context API.
pub fn transform_markdown(markdown: &str, width: usize, mode: &str) -> String {
    transform_markdown_with_context(markdown, width, mode, false, "assistant")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn visible(text: &str) -> String {
        pi_tui::strip_ansi_codes(text)
    }

    #[test]
    fn renders_supported_flowchart_as_inline_code_rows() {
        let rendered = transform_markdown_with_context(
            "Before\n\n```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```\nAfter",
            100,
            "streaming",
            false,
            "assistant",
        );
        let rendered = visible(&rendered);
        assert!(rendered.contains("┌───────┐"));
        assert!(rendered.contains("│ Start ├───▶│ Done │"));
        assert!(!rendered.contains("```mermaid"));
        assert!(rendered.contains("└───────┘    └──────┘`\nAfter"));
    }

    #[test]
    fn leaves_unsupported_and_oversized_syntax_unchanged() {
        let unsupported = "```mermaid\npie\n  title Pets\n```";
        assert_eq!(
            transform_markdown_with_context(unsupported, 100, "streaming", false, "assistant"),
            unsupported
        );

        let oversized = "```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```";
        assert_eq!(
            transform_markdown_with_context(oversized, 10, "streaming", false, "assistant"),
            oversized
        );
    }

    #[test]
    fn maps_semantic_spans_through_the_active_theme() {
        pi_tui::terminal_image::set_capabilities(pi_tui::terminal_image::TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        });
        tui_theme::load_theme("dark");
        let rendered = transform_markdown_with_context(
            "```mermaid\nflowchart LR\n  A --> B\n```",
            100,
            "streaming",
            false,
            "assistant",
        );
        assert!(rendered.contains("\x1b[38;2;80;80;80m"));
        assert!(visible(&rendered).contains("───▶"));
    }

    #[test]
    fn renders_incomplete_and_classed_flowcharts_during_streaming() {
        let incomplete = transform_markdown_with_context(
            "```mermaid\nflowchart LR\n  A --> B",
            100,
            "streaming",
            true,
            "assistant",
        );
        assert!(visible(&incomplete).contains("───▶"));

        let classed = "```mermaid\nflowchart LR\n  A[Foo]:::highlight --> B[Bar]\n```";
        let final_render =
            transform_markdown_with_context(classed, 100, "streaming", false, "assistant");
        assert!(final_render.contains(classed));
        assert!(final_render.contains("dropped, expected a link: \":::highlight --> B[Bar]\""));

        let streaming_render =
            transform_markdown_with_context(classed, 100, "streaming", true, "assistant");
        assert!(!streaming_render.contains("Mermaid diagram not rendered"));
        assert!(!streaming_render.contains("```mermaid"));
        assert!(visible(&streaming_render).contains("Foo"));
    }

    #[test]
    fn summarizes_multiple_partial_render_warnings() {
        let source = "```mermaid\nflowchart LR\n  A[Foo]:::highlight --> B[Bar]\n  C[Baz]:::other --> D[Qux]\n```";
        let rendered =
            transform_markdown_with_context(source, 100, "streaming", false, "assistant");
        assert!(rendered.contains("(+1 more)"));
        assert!(!rendered.contains("dropped, expected a link: \":::other --> D[Qux]\""));
    }

    #[test]
    fn chooses_a_safe_code_span_delimiter_for_backtick_labels() {
        let rendered = transform_markdown_with_context(
            "```mermaid\nflowchart LR\nA[one`tick] --> B[done]\n```",
            100,
            "streaming",
            false,
            "assistant",
        );
        let rendered = visible(&rendered);
        assert!(rendered.contains("``│ one`tick"));
        assert!(rendered.contains("done │``"));
    }

    #[test]
    fn respects_modes_and_skips_thinking_blocks() {
        let source = "```mermaid\nflowchart LR\nA --> B\n```";
        assert_eq!(
            transform_markdown_with_context(source, 100, "off", false, "assistant"),
            source
        );
        assert_eq!(
            transform_markdown_with_context(source, 100, "final", true, "assistant"),
            source
        );
        assert_eq!(
            transform_markdown_with_context(source, 100, "streaming", false, "assistant-thinking"),
            source
        );
        assert!(
            !transform_markdown_with_context(source, 100, "final", false, "assistant")
                .contains("```mermaid")
        );
    }
}
