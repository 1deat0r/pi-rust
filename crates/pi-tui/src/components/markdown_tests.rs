use crate::components::markdown::{plain_markdown_theme, Markdown, MarkdownOptions};
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
            calls2.lock().unwrap().push((source.to_string(), w));
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
    let calls = calls.lock().unwrap();
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
