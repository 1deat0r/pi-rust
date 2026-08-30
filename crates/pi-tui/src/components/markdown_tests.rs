#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test module
use crate::components::markdown::{
    parse_markdown, plain_markdown_theme, Block, Markdown, MarkdownOptions,
};
use crate::terminal_image::{reset_capabilities_cache, set_capabilities, TerminalCapabilities};
use crate::tui::Component;
use std::sync::Arc;

fn md(text: &str, width: usize) -> Vec<String> {
    let m = Markdown::new(text, 0, 0, plain_markdown_theme(), None, None);
    m.render(width)
        .into_iter()
        .map(|l| strip(&l).trim_end().to_string())
        .collect()
}

fn md_opts(text: &str, width: usize, options: MarkdownOptions) -> Vec<String> {
    let m = Markdown::new(text, 0, 0, plain_markdown_theme(), None, Some(options));
    m.render(width)
        .into_iter()
        .map(|l| strip(&l).trim_end().to_string())
        .collect()
}

fn strip(line: &str) -> String {
    // Remove ANSI SGR codes.
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[test]
fn renders_simple_nested_list() {
    let lines = md("- Item 1\n  - Nested 1.1\n  - Nested 1.2\n- Item 2", 80);
    assert!(lines.iter().any(|l| l.contains("- Item 1")));
    assert!(lines.iter().any(|l| l.contains("    - Nested 1.1")));
    assert!(lines.iter().any(|l| l.contains("    - Nested 1.2")));
    assert!(lines.iter().any(|l| l.contains("- Item 2")));
}

#[test]
fn preserves_lazy_blockquote_continuations_and_line_endings() {
    assert_eq!(md(">Foo\nbar", 80), vec!["│ Foo", "│ bar"]);
    assert_eq!(md(">Foo\n\nbar", 80), vec!["│ Foo", "", "bar"]);
    assert_eq!(md("first\r\nsecond", 80), vec!["first", "second"]);
}

#[test]
fn does_not_treat_hash_prefix_without_space_as_a_heading() {
    assert_eq!(md("#hashtag", 80), vec!["#hashtag"]);
}

#[test]
fn parses_unicode_without_slicing_inside_a_character() {
    let lines = md("MATRIX_UNICODE_日本語_🙂", 80);
    let rendered = lines.join("\n");
    assert!(rendered.contains("日本語"));
    assert!(rendered.contains("🙂"));
}

#[test]
fn renders_deeply_nested_list() {
    let lines = md("- Level 1\n  - Level 2\n    - Level 3\n      - Level 4", 80);
    assert!(lines.iter().any(|l| l.contains("- Level 1")));
    assert!(lines.iter().any(|l| l.contains("    - Level 2")));
    assert!(lines.iter().any(|l| l.contains("        - Level 3")));
    assert!(lines.iter().any(|l| l.contains("            - Level 4")));
}

#[test]
fn renders_ordered_nested_list() {
    let lines = md(
        "1. First\n   1. Nested first\n   2. Nested second\n2. Second",
        80,
    );
    assert!(lines.iter().any(|l| l.contains("1. First")));
    assert!(lines.iter().any(|l| l.contains("    1. Nested first")));
    assert!(lines.iter().any(|l| l.contains("    2. Nested second")));
    assert!(lines.iter().any(|l| l.contains("2. Second")));
}

#[test]
fn normalizes_ordered_list_markers_by_default() {
    let lines = md("1. alpha\n1. beta\n1. gamma", 80);
    assert_eq!(lines, vec!["1. alpha", "2. beta", "3. gamma"]);
}

#[test]
fn preserves_source_list_markers_when_configured() {
    let opts = MarkdownOptions {
        preserve_ordered_list_markers: true,
        ..Default::default()
    };
    let lines = md_opts(
        "  4. forth\n  3. third\n\n10) ten\n7) seven\n\n+ plus\n* star\n- minus\n+",
        80,
        opts,
    );
    assert_eq!(
        lines,
        vec![
            "4. forth", "3. third", "", "10) ten", "7) seven", "", "+ plus", "* star", "- minus",
            "+",
        ]
    );
}

#[test]
fn renders_mixed_ordered_and_unordered_lists() {
    let lines = md("1. Ordered item\n   - Unordered nested\n   - Another nested\n2. Second ordered\n   - More nested", 80);
    assert!(lines.iter().any(|l| l.contains("1. Ordered item")));
    assert!(lines.iter().any(|l| l.contains("    - Unordered nested")));
    assert!(lines.iter().any(|l| l.contains("2. Second ordered")));
}

#[test]
fn renders_blank_lines_between_loose_list_items() {
    let lines = md("1. Lorem ipsum dolor sit amet.\n\n   Ut enim ad minim veniam.\n\n2. Duis aute irure dolor.\n\n   Excepteur sint occaecat cupidatat.\n\n3. Beep boop", 80);
    assert_eq!(
        lines,
        vec![
            "1. Lorem ipsum dolor sit amet.",
            "",
            "   Ut enim ad minim veniam.",
            "",
            "2. Duis aute irure dolor.",
            "",
            "   Excepteur sint occaecat cupidatat.",
            "",
            "3. Beep boop",
        ]
    );
}

#[test]
fn renders_task_list_markers() {
    let lines = md("- [ ] beep\n- [x] boop", 80);
    assert_eq!(lines, vec!["- [ ] beep", "- [x] boop"]);
}

#[test]
fn normalizes_uppercase_checked_task_markers() {
    assert_eq!(md("- [X] done", 80), vec!["- [x] done"]);
}

#[test]
fn maintains_numbering_when_code_blocks_are_not_indented() {
    let lines = md("1. First item\n\n```typescript\n// code block\n```\n\n2. Second item\n\n```typescript\n// another code block\n```\n\n3. Third item", 80);
    let numbered: Vec<&String> = lines
        .iter()
        .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
        .collect();
    // The list items + code markers both appear; check item numbers exist.
    assert!(lines.iter().any(|l| l.contains("1. First item")));
    assert!(lines.iter().any(|l| l.contains("2. Second item")));
    assert!(lines.iter().any(|l| l.contains("3. Third item")));
    assert_eq!(numbered.len(), 3);
}

#[test]
fn indents_wrapped_unordered_list_lines() {
    let lines = md("- alpha beta gamma delta epsilon", 20);
    assert_eq!(lines, vec!["- alpha beta gamma", "  delta epsilon"]);
}

#[test]
fn indents_wrapped_ordered_list_lines() {
    let lines = md("1. alpha beta gamma delta epsilon", 20);
    assert_eq!(lines, vec!["1. alpha beta gamma", "   delta epsilon"]);
}

#[test]
fn indents_wrapped_ordered_list_lines_with_multidigit_markers() {
    let lines = md("10. alpha beta gamma delta epsilon", 21);
    assert_eq!(lines, vec!["10. alpha beta gamma", "    delta epsilon"]);
}

#[test]
fn indents_wrapped_nested_list_lines() {
    let lines = md("- parent\n  - alpha beta gamma delta epsilon", 24);
    assert_eq!(
        lines,
        vec!["- parent", "    - alpha beta gamma", "      delta epsilon"]
    );
}

#[test]
fn renders_blockquote_inside_list_item() {
    let lines = md("- > alpha beta gamma delta epsilon zeta", 24);
    assert_eq!(
        lines,
        vec!["- │ alpha beta gamma", "  │ delta epsilon zeta"]
    );
}

#[test]
fn renders_code_inside_list_item() {
    let lines = md("- ```ts\n  alpha beta gamma delta epsilon zeta\n  ```", 24);
    assert_eq!(
        lines,
        vec![
            "- ```ts",
            "    alpha beta gamma",
            "  delta epsilon zeta",
            "  ```"
        ]
    );
}

#[test]
fn renders_simple_table() {
    let lines = md(
        "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |",
        80,
    );
    assert!(lines.iter().any(|l| l.contains("Name")));
    assert!(lines.iter().any(|l| l.contains("Alice")));
    assert!(lines.iter().any(|l| l.contains("│")));
    assert!(lines.iter().any(|l| l.contains("─")));
}

#[test]
fn renders_row_dividers_between_data_rows() {
    let lines = md(
        "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |",
        80,
    );
    let dividers = lines.iter().filter(|l| l.contains("┼")).count();
    assert_eq!(dividers, 2, "Expected header + row divider");
}

#[test]
fn keeps_column_width_at_longest_word() {
    let longest_word = "superlongword";
    let lines = md(&format!("| Column One | Column Two |\n| --- | --- |\n| {longest_word} short | otherword |\n| small | tiny |"), 32);
    let data_line = lines
        .iter()
        .find(|l| l.contains(longest_word))
        .expect("data row with longest word");
    let first_segment = data_line.split('│').nth(1).unwrap();
    let first_width = first_segment.trim().len();
    assert!(
        first_width >= longest_word.len(),
        "col width {first_width} < {}",
        longest_word.len()
    );
}

#[test]
fn wraps_table_cells_when_table_exceeds_available_width() {
    let lines = md("| Command | Description | Example |\n| --- | --- | --- |\n| npm install | Install all dependencies | npm install |\n| npm run build | Build the project | npm run build |", 50);
    for line in &lines {
        assert!(
            line.chars().count() <= 50,
            "Line exceeds width 50: {line:?}"
        );
    }
    let all_text = lines.join(" ");
    assert!(all_text.contains("Command"));
    assert!(all_text.contains("Description"));
    assert!(all_text.contains("npm install"));
}

#[test]
fn parses_tables_without_outer_pipes() {
    let lines = md("Name | Age\n--- | ---\nAlice | 30", 80);
    assert!(lines.iter().any(|line| line.contains("Name")));
    assert!(lines.iter().any(|line| line.contains("Alice")));
    assert!(lines.iter().any(|line| line.contains('│')));
}

#[test]
fn keeps_escaped_pipes_inside_table_cells() {
    let lines = md("| Expr | Value |\n| --- | --- |\n| a \\| b | x |", 80);
    assert!(lines.iter().any(|line| line.contains("a | b")));
    assert!(!lines.iter().any(|line| line.contains("a \\")));
}

#[test]
fn preserves_backticks_inside_single_dollar_math() {
    let source = "literal $x`y$";
    assert_eq!(md(source, 80), vec![source]);
}

#[test]
fn renders_inline_double_dollar_math() {
    assert_eq!(md("before $$x^2$$ after", 80), vec!["before x² after"]);
}

#[test]
fn inserts_upstream_spacing_between_adjacent_block_tokens() {
    assert_eq!(
        md("```text\ncode\n```\n# title", 80),
        vec!["```text", "  code", "```", "", "title"]
    );
    assert_eq!(
        md("| A |\n|---|\n| 1 |\n# title", 80),
        vec!["┌───┐", "│ A │", "├───┤", "│ 1 │", "└───┘", "", "title"]
    );
}

#[test]
fn parses_marked_single_column_tables_without_outer_pipes() {
    let lines = md("Header\n:---\nValue", 80);
    assert!(lines.iter().any(|line| line.contains("Header")));
    assert!(lines.iter().any(|line| line.contains("Value")));
    assert!(lines.iter().any(|line| line.contains('│')));
}

#[test]
fn does_not_stall_or_create_a_table_for_mismatched_columns() {
    let tokens = parse_markdown("A | B\n--- | --- | ---\nnot a table");
    assert!(tokens.iter().all(|token| !matches!(token, Block::Table(_))));
}

#[test]
fn rejects_marked_table_separators_without_hyphens() {
    for source in ["Header\n:\nValue", "Header\n---\nValue"] {
        let tokens = parse_markdown(source);
        assert!(tokens.iter().all(|token| !matches!(token, Block::Table(_))));
    }
}

#[test]
fn renders_heading_with_trailing_spacing() {
    let lines = md("# Hello", 80);
    assert_eq!(lines[0], "Hello");
}

#[test]
fn caches_transformed_markdown_by_source_and_width() {
    use std::sync::Mutex;
    let calls: Arc<Mutex<Vec<(String, usize)>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
    let calls2 = calls.clone();
    let options = MarkdownOptions {
        transform: Some(Box::new(move |source: &str, w: usize| {
            calls2
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((source.to_string(), w));
            format!("{source} {w}")
        })),
        ..Default::default()
    };
    // Note: Arc is accessible because std::sync::Arc imported above.
    let mut m = Markdown::new("source", 2, 0, plain_markdown_theme(), None, Some(options));
    let rendered = m.render(80);
    assert_eq!(rendered[0].trim(), "source 76");
    m.render(80);
    m.render(60);
    m.set_text("updated");
    m.render(60);
    m.invalidate();
    m.render(60);
    let calls = calls.lock().unwrap_or_else(|error| error.into_inner());
    let total = calls.len();
    assert_eq!(total, 4);
}

#[test]
fn renders_code_blocks_and_headings() {
    let lines = md("# Title\n\n```rust\nfn main() {}\n```", 80);
    assert_eq!(lines[0], "Title");
    assert!(lines.iter().any(|l| l.trim() == "```rust"));
    assert!(lines.iter().any(|l| l == "  fn main() {}"));
}

#[test]
fn marked_edge_snapshot_is_stable_for_mixed_blocks() {
    let lines = md(
        "# Title\n\n- [x] done\n\n```rust\nlet x = 1;\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |",
        40,
    );
    assert_eq!(
        lines,
        vec![
            "Title",
            "",
            "- [x] done",
            "",
            "```rust",
            "  let x = 1;",
            "```",
            "",
            "┌───┬───┐",
            "│ A │ B │",
            "├───┼───┤",
            "│ 1 │ 2 │",
            "└───┴───┘",
        ]
    );
}

#[test]
fn stabilizes_streaming_fences_and_display_math_shapes() {
    assert_eq!(
        md("```ts\nconst x = 1;\n``", 80),
        vec!["```ts", "  const x = 1;", "```"]
    );
    assert_eq!(
        md("Before\n\n$$x^2$$\n\nafter", 80),
        vec!["Before", "", "x²", "", "after"]
    );
    assert_eq!(md("\\[\nx^2", 80), vec!["\\[", "x^2"]);
}

#[test]
fn follows_marked_display_math_body_and_closing_line_shapes() {
    assert_eq!(md("$$ x^2 $$", 80), vec!["x²"]);
    assert_eq!(md("$$x^2\nx+1 $$", 80), vec!["x² x+1"]);
    assert_eq!(
        md("Before\n\n$$\nx^2 $$\n\nafter", 80),
        vec!["Before", "", "x²", "", "after"]
    );
}

#[test]
fn renders_backslash_hard_breaks_as_real_line_breaks() {
    assert_eq!(md("first\\\nsecond", 80), vec!["first", "second"]);
}

#[test]
fn ignores_escaped_backslash_latex_closers_while_scanning() {
    let tokens = crate::components::markdown::parse_markdown(r"Map \(a \\) b\)");
    let crate::components::markdown::Block::Paragraph(tokens) = &tokens[0] else {
        panic!("expected a paragraph");
    };
    assert!(matches!(
        tokens.as_slice(),
        [crate::components::markdown::Inline::Text(prefix),
            crate::components::markdown::Inline::Latex { text, pending: false, .. }]
            if prefix == "Map " && text == r"a \\) b"
    ));
}

#[test]
fn autolinks_follow_marked_fallback_and_osc8_shapes() {
    set_capabilities(TerminalCapabilities {
        images: None,
        true_color: false,
        hyperlinks: false,
    });
    let fallback = Markdown::new(
        "Contact user@example.com or https://example.com.",
        0,
        0,
        plain_markdown_theme(),
        None,
        None,
    )
    .render(80)
    .join(" ");
    assert!(fallback.contains("user@example.com"));
    assert!(fallback.contains("https://example.com"));
    assert_eq!(fallback.matches("https://example.com").count(), 1);

    set_capabilities(TerminalCapabilities {
        images: None,
        true_color: false,
        hyperlinks: true,
    });
    let linked = Markdown::new(
        "[docs](https://example.com)",
        0,
        0,
        plain_markdown_theme(),
        None,
        None,
    )
    .render(80)
    .join("");
    assert!(linked.contains("\x1b]8;;https://example.com\x1b\\"));
    reset_capabilities_cache();
}

#[test]
fn ragged_marked_table_is_normalized_without_overflow() {
    let lines = md("| A | B |\n|---|---|\n| only |\n| too | many | cells |", 20);
    assert!(!lines.is_empty());
    assert!(lines.iter().all(|line| line.chars().count() <= 20));
}
