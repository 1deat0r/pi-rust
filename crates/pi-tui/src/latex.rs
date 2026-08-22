//! LaTeX math rendering — port of `packages/tui/src/latex.ts`.
//!
//! Renders a basic LaTeX math expression as terminal-friendly Unicode text.
//! Returns `None` when the expression contains unsupported or malformed
//! syntax. Symbol/operator tables live in `latex_tables.rs` (generated from
//! the upstream file).

#[path = "latex_tables.rs"]
mod latex_tables;

use crate::utils::visible_width;


// Private-use markers used by the upstream renderer.
const NAMED_OPERATOR_START: &str = "\u{f0004}";
const NAMED_OPERATOR_END: &str = "\u{f0005}";
const LAYOUT_MARKER_START: &str = "\u{f0000}";
const LAYOUT_MARKER_END: &str = "\u{f0001}";
const TRAILING_LAYOUT_MARKER: &str = "\u{f0000}0\u{f0001}$";
const PROTECTED_SPACE: &str = "\u{f0002}";
const NEGATIVE_SPACE: &str = "\u{0}";

fn table_lookup<'a>(table: &'a [(&'static str, &'static str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn set_contains(table: &[&str], key: &str) -> bool {
    table.contains(&key)
}

fn is_ascii_letter(c: char) -> bool {
    c.is_ascii_alphabetic()
}

/// Replace every char through a replacements table; None when unmapped.
fn replace_characters(value: &str, replacements: &[(&'static str, &'static str)]) -> Option<String> {
    let mut result = String::new();
    for character in value.chars() {
        let ch = character.to_string();
        let replacement = table_lookup(replacements, &ch)?;
        result.push_str(replacement);
    }
    Some(result)
}

/// Upstream `formatScript`: try Unicode sub/superscripts, else fall back.
fn format_script(value: &str, kind: ScriptKind) -> String {
    let value = value.trim();
    let replacements = if kind == ScriptKind::Sub { subscripts() } else { superscripts() };
    // value.replace(/\s*([=+-])\s*/g, "$1")
    let mut normalized = String::new();
    for c in value.chars() {
        if c == '=' || c == '+' || c == '-' {
            if normalized.ends_with(' ') {
                normalized.pop();
            }
            normalized.push(c);
        } else {
            if c.is_whitespace() {
                // squeeze whitespace runs around operators; otherwise drop
                if normalized.ends_with(' ') {
                    continue;
                }
                normalized.push(' ');
            } else {
                normalized.push(c);
            }
        }
    }
    // trailing whitespace
    while normalized.ends_with(' ') {
        normalized.pop();
    }
    let unicode = replace_characters(&normalized, replacements);
    if let Some(u) = unicode {
        return u;
    }

    let prefix = if kind == ScriptKind::Sub { "_" } else { "^" };
    let char_count = value.chars().count();
    if char_count == 1 || (kind == ScriptKind::Sub && value.chars().all(|c| c.is_ascii_alphabetic())) {
        return format!("{prefix}{value}");
    }
    format!("{prefix}({value})")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptKind {
    Sub,
    Sup,
}

/// Upstream `formatFraction`.
fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = numerator.trim();
    let denominator = denominator.trim();
    let simple_numerator = simple_term(numerator);
    // Upstream: denominator is simple when all digits/dots OR a single char.
    let simple_denominator =
        (!denominator.is_empty() && denominator.chars().all(|c| c.is_numeric() || c == '.'))
            || denominator.chars().count() == 1;
    let num = if simple_numerator { numerator.to_string() } else { format!("({numerator})") };
    let den = if simple_denominator { denominator.to_string() } else { format!("({denominator})") };
    format!("{num}/{den}")
}

fn simple_term(value: &str) -> bool {
    // /^[\p{L}\p{N}.]+$/u
    !value.is_empty() && value.chars().all(|c| c.is_alphanumeric() || c == '.')
}

/// Upstream `formatRoot`.
fn format_root(value: &str, symbol: &str) -> String {
    let value = value.trim();
    if simple_term(value) {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

fn subscripts() -> &'static [(&'static str, &'static str)] {
    latex_tables::SUBSCRIPTS
}
fn superscripts() -> &'static [(&'static str, &'static str)] {
    latex_tables::SUPERSCRIPTS
}

/// A layout node produced while parsing (fractions/operators/matrices).
#[derive(Debug, Clone)]
enum LayoutNode {
    Fraction { numerator: String, denominator: String },
    Operator { operator: String, lower: Option<String>, upper: Option<String> },
    Matrix { lines: Vec<String>, baseline: usize },
}

#[derive(Debug, Clone)]
struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{}{}", " ".repeat(left), line, " ".repeat(padding - left))
}

fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout { lines: vec![String::new()], width: 0, baseline: 0 };
    }
    let baseline = layouts.iter().map(|l| l.baseline).max().unwrap_or(0);
    let below = layouts
        .iter()
        .map(|l| l.lines.len().saturating_sub(l.baseline + 1))
        .max()
        .unwrap_or(0);
    let total_width: usize = layouts.iter().map(|l| l.width).sum();
    let mut lines: Vec<String> = Vec::new();
    for row in 0..=baseline + below {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row as isize - baseline as isize + layout.baseline as isize;
            if source_row >= 0 && (source_row as usize) < layout.lines.len() {
                line.push_str(&pad_layout_line(
                    layout.lines.get(source_row as usize).map(|s| s.as_str()).unwrap_or(""),
                    layout.width,
                    false,
                ));
            } else {
                line.push_str(&" ".repeat(layout.width));
            }
        }
        lines.push(line.trim_end().to_string());
    }
    Layout { lines, width: total_width, baseline }
}

/// Render layout markers in a source line into a fully laid-out block.
fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0usize;

    for source_line in source.split('\n') {
        let mut layouts: Vec<Layout> = Vec::new();
        let mut position = 0usize;
        let mut previous_node: Option<&LayoutNode> = None;

        // Find all \u{f0000}<digits>\u{f0001} markers.
                let s = source_line;
        let mut search_from = 0usize;
        loop {
            let marker = s[search_from..].find(LAYOUT_MARKER_START);
            let Some(rel) = marker else { break };
            let mstart = search_from + rel;
            // Read digits until LAYOUT_MARKER_END.
            let digits_start = mstart + LAYOUT_MARKER_START.len();
            let Some(digits_end_rel) = s[digits_start..].find(LAYOUT_MARKER_END) else { break };
            let digits_end = digits_start + digits_end_rel;
            let digits = &s[digits_start..digits_end];
            let Some(index) = digits.parse::<usize>().ok() else { break };
            let marker_len = digits_end + LAYOUT_MARKER_END.len() - mstart;
            search_from = digits_end + LAYOUT_MARKER_END.len();

            let Some(node) = nodes.get(index) else { continue };

            if mstart > position {
                let sliced = &s[position..mstart];
                let trimmed = if previous_node.is_some() {
                    sliced.trim_start().trim_end()
                } else {
                    sliced.trim_end()
                };
                let preserve_leading = matches!(previous_node, Some(LayoutNode::Matrix { .. })) && sliced.starts_with(' ');
                let preserve_trailing = matches!(node, LayoutNode::Matrix { .. }) && sliced.ends_with(' ');
                let text = if !trimmed.is_empty() {
                    format!(
                        "{}{}{}",
                        if preserve_leading { " " } else { "" },
                        trimmed,
                        if preserve_trailing { " " } else { "" }
                    )
                } else if preserve_leading || preserve_trailing {
                    " ".to_string()
                } else {
                    String::new()
                };
                layouts.push(Layout { lines: vec![text.clone()], width: visible_width(&text), baseline: 0 });
            }

            match node {
                LayoutNode::Fraction { numerator, denominator } => {
                    let numerator_layout = render_layout(numerator, nodes);
                    let denominator_layout = render_layout(denominator, nodes);
                    let content_width = numerator_layout.width.max(denominator_layout.width).max(1);
                    let width = content_width + 2;
                    let mut lines: Vec<String> = Vec::new();
                    for line in &numerator_layout.lines {
                        lines.push(pad_layout_line(line, width, true));
                    }
                    lines.push(format!(" {} ", "─".repeat(content_width)));
                    for line in &denominator_layout.lines {
                        lines.push(pad_layout_line(line, width, true));
                    }
                    layouts.push(Layout { lines, width, baseline: numerator_layout.lines.len() });
                }
                LayoutNode::Operator { operator, lower, upper } => {
                    let content_width = visible_width(operator)
                        .max(lower.as_ref().map(|s| visible_width(s)).unwrap_or(0))
                        .max(upper.as_ref().map(|s| visible_width(s)).unwrap_or(0));
                    let mut lines: Vec<String> = Vec::new();
                    if let Some(upper) = upper {
                        lines.push(format!("{} ", pad_layout_line(upper, content_width, true)));
                    }
                    lines.push(format!("{} ", pad_layout_line(operator, content_width, true)));
                    if let Some(lower) = lower {
                        lines.push(format!("{} ", pad_layout_line(lower, content_width, true)));
                    }
                    layouts.push(Layout {
                        lines,
                        width: content_width + 1,
                        baseline: if upper.is_some() { 1 } else { 0 },
                    });
                }
                LayoutNode::Matrix { lines: node_lines, baseline } => {
                    let width = node_lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
                    let laid: Vec<String> = node_lines.iter().map(|l| pad_layout_line(l, width, false)).collect();
                    layouts.push(Layout { lines: laid, width, baseline: *baseline });
                }
            }
            position = mstart + marker_len;
            previous_node = Some(node);
        }

        if position < s.len() {
            let sliced = &s[position..];
            let trimmed = if previous_node.is_some() { sliced.trim_start() } else { sliced };
            let text = if matches!(previous_node, Some(LayoutNode::Matrix { .. })) && sliced.starts_with(' ') {
                format!(" {trimmed}")
            } else {
                trimmed.to_string()
            };
            layouts.push(Layout { lines: vec![text.clone()], width: visible_width(&text), baseline: 0 });
        }

        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }

    let width = rendered_lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
    Layout {
        lines: rendered_lines,
        width,
        baseline: first_baseline,
    }
}

struct LatexParser<'a> {
    source: &'a str,
    layout_nodes: &'a mut Vec<LayoutNode>,
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
}

impl<'a> LatexParser<'a> {
    fn new(source: &'a str, layout_nodes: &'a mut Vec<LayoutNode>, display: bool) -> Self {
        Self { source, layout_nodes, display, position: 0, supported: true, stack_fractions: true }
    }

    fn render(&mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.source.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    fn parse_sequence(&mut self, end_character: Option<char>) -> String {
        let mut result = String::new();
        let chars: Vec<char> = self.source.chars().collect();
        while self.position < chars.len() {
            let character = chars[self.position];
            if let Some(end) = end_character {
                if character == end {
                    self.position += 1;
                    return result;
                }
            }

            if character == '}' {
                self.supported = false;
                return result;
            }

            if character == '{' {
                self.position += 1;
                result.push_str(&self.parse_sequence(Some('}')));
                continue;
            }

            if character == '\\' {
                let command = self.parse_command();
                if command == NEGATIVE_SPACE {
                    while result.ends_with(' ') {
                        result.pop();
                    }
                    if result.ends_with(NAMED_OPERATOR_END) {
                        result.truncate(result.len() - NAMED_OPERATOR_END.len());
                    }
                } else {
                    result.push_str(&command);
                }
                continue;
            }

            if character == '^' || character == '_' {
                self.position += 1;
                while result.ends_with(' ') {
                    result.pop();
                }
                let script = format_script(
                    &self.parse_required_argument(false),
                    if character == '_' { ScriptKind::Sub } else { ScriptKind::Sup },
                );
                if result.ends_with(NAMED_OPERATOR_END) {
                    let end_len = NAMED_OPERATOR_END.len();
                    result.truncate(result.len() - end_len);
                    result.push_str(&script);
                    result.push_str(NAMED_OPERATOR_END);
                } else {
                    result.push_str(&script);
                }
                continue;
            }

            if character.is_whitespace() {
                result.push_str(&self.parse_whitespace());
                continue;
            }

            if character == '=' || character == '<' || character == '>' {
                while result.ends_with(' ') {
                    result.pop();
                }
                result.push_str(&format!(" {} ", character));
                self.position += 1;
                continue;
            }

            if character == '&' {
                self.position += 1;
                continue;
            }

            if character == '~' {
                self.position += 1;
                result.push(' ');
                continue;
            }

            if character == '.' {
                // Trailing layout marker handling (matrix '.' mutation).
                if let Some(rel) = result.rfind(TRAILING_LAYOUT_MARKER) {
                    let marker_start = rel;
                    let digits_end = result[marker_start + LAYOUT_MARKER_START.len()..].find(LAYOUT_MARKER_END);
                    let node_idx: usize = result[marker_start + LAYOUT_MARKER_START.len()..][..(digits_end.unwrap_or(0))]
                        .parse()
                        .unwrap_or(usize::MAX);
                    if let Some(LayoutNode::Matrix { lines, .. }) = self.layout_nodes.get_mut(node_idx) {
                        if let Some(last_line) = lines.last_mut() {
                            last_line.push(character);
                            self.position += 1;
                            continue;
                        }
                    }
                }
            }

            result.push(character);
            self.position += 1;
        }

        if end_character.is_some() {
            self.supported = false;
        }
        result
    }

    fn parse_whitespace(&mut self) -> String {
        while self.position < self.source.len()
            && self.source[self.position..].chars().next().map(|c| c.is_whitespace()).unwrap_or(false)
        {
            self.position += 1;
        }
        " ".to_string()
    }

    fn parse_command(&mut self) -> String {
        self.position += 1; // skip '\\'
        if self.position >= self.source.len() {
            self.supported = false;
            return String::new();
        }

        let chars: Vec<char> = self.source.chars().collect();
        let first = chars[self.position];
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.position < chars.len() && chars[self.position] == '\n' {
                self.position += 1;
            }
            return " ".to_string();
        }
        let command;
        if is_ascii_letter(first) {
            let start = self.position;
            while self.position < chars.len() && is_ascii_letter(chars[self.position]) {
                self.position += 1;
            }
            command = self.source[start..self.position].to_string();
        } else {
            command = first.to_string();
            self.position += 1;
        }

        if command == "\\" {
            return "\n".to_string();
        }
        if set_contains(latex_tables::SPACING_COMMANDS, &command) {
            return " ".to_string();
        }
        if set_contains(latex_tables::NEGATIVE_SPACING_COMMANDS, &command) {
            return NEGATIVE_SPACE.to_string();
        }
        if set_contains(latex_tables::IGNORED_COMMANDS, &command) {
            return String::new();
        }
        if matches!(command.as_str(), "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return command;
        }
        if command == "|" {
            return "‖".to_string();
        }
        if command == "not" {
            let value = self.parse_required_argument(true).trim().to_string();
            if let Some(negated) = table_lookup(latex_tables::NEGATED_SYMBOLS, &value) {
                return format!(" {negated} ");
            }
            let mut characters = value.chars();
            let Some(first_char) = characters.next() else {
                self.supported = false;
                return String::new();
            };
            let rest: String = characters.collect();
            return format!(" {first_char}\u{338}{rest} ");
        }
        if set_contains(latex_tables::LIMIT_OPERATORS, &command) {
            return self.parse_operator(&command, InlineLowerStyle::Bracket, true, true);
        }

        if let Some(symbol) = table_lookup(latex_tables::SYMBOLS, &command) {
            if set_contains(latex_tables::DISPLAY_LIMIT_SYMBOLS, &command) {
                return self.parse_operator(symbol, InlineLowerStyle::Script, true, false);
            }
            return if matches!(command.as_str(), "cdot" | "times") || set_contains(latex_tables::RELATION_COMMANDS, &command) {
                format!(" {symbol} ")
            } else {
                symbol.to_string()
            };
        }
        if set_contains(latex_tables::NAMED_OPERATORS, &command) {
            return format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}");
        }
        if set_contains(latex_tables::SIZE_COMMANDS, &command) {
            return String::new();
        }
        if command == "left" || command == "middle" || command == "right" {
            if self.source[self.position..].starts_with('.') {
                self.position += 1;
            }
            return String::new();
        }
        if matches!(command.as_str(), "frac" | "dfrac" | "tfrac") {
            let should_stack = self.display && self.stack_fractions && command != "tfrac";
            let numerator = self.parse_required_argument(!should_stack);
            let denominator = self.parse_required_argument(!should_stack);
            if should_stack {
                self.layout_nodes.push(LayoutNode::Fraction {
                    numerator: normalize_output(&numerator),
                    denominator: normalize_output(&denominator),
                });
                return format!("{LAYOUT_MARKER_START}{}{LAYOUT_MARKER_END}", self.layout_nodes.len() - 1);
            }
            return format_fraction(&numerator, &denominator);
        }
        if command == "sqrt" {
            let degree = self.parse_optional_argument().map(|d| d.trim().to_string());
            let value = self.parse_required_argument(true);
            match degree.as_deref() {
                None | Some("2") => return format_root(&value, "√"),
                Some("3") => return format_root(&value, "∛"),
                Some("4") => return format_root(&value, "∜"),
                Some(d) => {
                    let degree_script = format_script(d, ScriptKind::Sup);
                    return format!("{degree_script}{}", format_root(&value, "√"));
                }
            }
        }
        if command == "boxed" || command == "fbox" {
            return format!("[{}]", self.parse_required_argument(true).trim());
        }
        if matches!(command.as_str(), "binom" | "dbinom" | "tbinom") {
            return format!(
                "({} choose {})",
                self.parse_required_argument(true).trim(),
                self.parse_required_argument(true).trim()
            );
        }
        if let Some(accent) = table_lookup(latex_tables::ACCENTS, &command) {
            let value = self.parse_required_argument(true);
            return if value.chars().count() == 1 {
                format!("{value}{accent}")
            } else {
                format!("{command}({value})")
            };
        }
        if command == "mathbb" {
            let value = self.parse_required_argument(true);
            return value.chars().map(|c| {
                let ch = c.to_string();
                table_lookup(latex_tables::BLACKBOARD, &ch).unwrap_or(&ch).to_string()
            }).collect();
        }
        if command == "operatorname" {
            let starred = self.source[self.position..].starts_with('*');
            if starred {
                self.position += 1;
            }
            let operator = normalize_output(&self.parse_required_argument(true)).trim().to_string();
            return self.parse_operator(&operator, InlineLowerStyle::Bracket, starred, true);
        }
        if command == "mod" || command == "bmod" {
            return " mod ".to_string();
        }
        if command == "pmod" || command == "pod" {
            let value = self.parse_required_argument(true).trim().to_string();
            return if command == "pmod" {
                format!(" (mod {value})")
            } else {
                format!(" ({value})")
            };
        }
        if command == "overset" || command == "stackrel" {
            let upper = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_string();
            return format!("{value}{}", format_script(&upper, ScriptKind::Sup));
        }
        if command == "underset" {
            let lower = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_string();
            return format!("{value}{}", format_script(&lower, ScriptKind::Sub));
        }
        if set_contains(latex_tables::PLAIN_WRAPPERS, &command) {
            let value = self.parse_required_argument(true);
            return if command.starts_with("text") || command == "mbox" {
                value
            } else {
                value.trim().to_string()
            };
        }
        if command == "begin" {
            return self.parse_environment();
        }
        if command == "end" {
            self.supported = false;
            return String::new();
        }

        self.supported = false;
        format!("\\{command}")
    }

    fn parse_operator(
        &mut self,
        operator: &str,
        inline_lower_style: InlineLowerStyle,
        display_limits: bool,
        spaced: bool,
    ) -> String {
        let mut use_display_limits = display_limits;
        // Optional \limits / \nolimits modifier.
        let mut check_pos = self.position;
        while check_pos < self.source.len()
            && self.source[check_pos..].chars().next().map(|c| c == ' ' || c == '\t').unwrap_or(false)
        {
            check_pos += 1;
        }
        let rest = &self.source[check_pos..];
        if let Some(rel) = rest.find('\\') {
            let after = &rest[rel + 1..];
            if after.starts_with("limits") && !after[6..].chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
                use_display_limits = true;
                self.position = check_pos + rel + 1 + 6;
            } else if after.starts_with("nolimits") && !after[8..].chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
                use_display_limits = false;
                self.position = check_pos + rel + 1 + 8;
            }
        }

        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_pos = self.position;
            while script_pos < self.source.len()
                && self.source[script_pos..].chars().next().map(|c| c == ' ' || c == '\t').unwrap_or(false)
            {
                script_pos += 1;
            }
            let kind = self.source[script_pos..].chars().next();
            if !matches!(kind, Some('_') | Some('^')) {
                break;
            }
            self.position = script_pos + 1;
            let value = normalize_output(&self.parse_required_argument(false)).replace(' ', "");
            match kind {
                Some('_') => {
                    if lower.is_some() {
                        self.supported = false;
                    }
                    lower = Some(value);
                }
                Some('^') => {
                    if upper.is_some() {
                        self.supported = false;
                    }
                    upper = Some(value);
                }
                _ => {}
            }
        }

        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            self.layout_nodes.push(LayoutNode::Operator {
                operator: operator.to_string(),
                lower,
                upper,
            });
            return format!("{LAYOUT_MARKER_START}{}{LAYOUT_MARKER_END}", self.layout_nodes.len() - 1);
        }

        let mut rendered = operator.to_string();
        if let Some(lower) = &lower {
            rendered.push_str(&match inline_lower_style {
                InlineLowerStyle::Bracket => format!("[{lower}]"),
                InlineLowerStyle::Script => format_script(lower, ScriptKind::Sub),
            });
        }
        if let Some(upper) = &upper {
            rendered.push_str(&format_script(upper, ScriptKind::Sup));
        }
        if spaced {
            format!(" {rendered} ")
        } else {
            rendered
        }
    }

    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    fn parse_required_argument_value(&mut self) -> String {
        while self.position < self.source.len()
            && self.source[self.position..].chars().next().map(|c| c.is_whitespace()).unwrap_or(false)
        {
            self.position += 1;
        }
        if self.position >= self.source.len() {
            self.supported = false;
            return String::new();
        }
        let ch = self.source[self.position..].chars().next().unwrap();
        if ch == '{' {
            self.position += 1;
            return self.parse_sequence(Some('}'));
        }
        if ch == '\\' {
            return self.parse_command();
        }
        self.position += ch.len_utf8();
        ch.to_string()
    }

    fn parse_optional_argument(&mut self) -> Option<String> {
        while self.position < self.source.len()
            && self.source[self.position..].chars().next().map(|c| c == ' ' || c == '\t').unwrap_or(false)
        {
            self.position += 1;
        }
        if !self.source[self.position..].starts_with('[') {
            return None;
        }
        let Some(rel) = self.source[self.position + 1..].find(']') else {
            self.supported = false;
            return None;
        };
        let end = self.position + 1 + rel;
        let value = self.source[self.position + 1..end].to_string();
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    fn read_raw_group(&mut self) -> Option<String> {
        while self.position < self.source.len()
            && self.source[self.position..].chars().next().map(|c| c == ' ' || c == '\t').unwrap_or(false)
        {
            self.position += 1;
        }
        if !self.source[self.position..].starts_with('{') {
            self.supported = false;
            return None;
        }
        self.position += 1;
        let start = self.position;
        let mut depth = 1usize;
        let chars: Vec<char> = self.source.chars().collect();
        while self.position < chars.len() {
            let c = chars[self.position];
            if c == '\\' {
                self.position += 2;
                continue;
            }
            if c == '{' {
                depth += 1;
            }
            if c == '}' {
                depth -= 1;
                if depth == 0 {
                    let value = self.source[start..self.position].to_string();
                    self.position += 1;
                    return Some(value);
                }
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    fn split_environment_rows(&self, body: &str) -> Vec<String> {
        // Split on \\ optionally followed by [..]
        let mut rows = Vec::new();
        let mut start = 0usize;
        let bytes_len = body.len();
        let mut i = 0usize;
        while i + 1 < bytes_len {
            if body.as_bytes()[i] == b'\\' && body.as_bytes()[i + 1] == b'\\' {
                let mut j = i + 2;
                if j < bytes_len && body.as_bytes()[j] == b'[' {
                    while j < bytes_len && body.as_bytes()[j] != b']' {
                        j += 1;
                    }
                    if j < bytes_len {
                        j += 1;
                    }
                }
                rows.push(body[start..i].to_string());
                i = j;
                start = i;
                continue;
            }
            i += 1;
        }
        rows.push(body[start..].to_string());
        rows
    }

    fn parse_environment(&mut self) -> String {
        let environment = self.read_raw_group().unwrap_or_default();
        if environment.is_empty() {
            return String::new();
        }
        let end_marker = format!("\\end{{{environment}}}");
        let Some(end) = self.source[self.position..].find(&end_marker) else {
            self.supported = false;
            return String::new();
        };
        let end = self.position + end;
        let body = self.source[self.position..end].to_string();
        self.position = end + end_marker.len();

        if environment == "equation" || environment == "equation*" || environment == "displaymath" {
            return self.render_nested(&body, true).trim().to_string();
        }

        if matches!(
            environment.as_str(),
            "aligned" | "align" | "align*" | "alignedat" | "alignat" | "alignat*"
                | "gather" | "gathered" | "multline" | "multline*" | "split"
        ) {
            let aligned_at = matches!(environment.as_str(), "alignedat" | "alignat" | "alignat*");
            let aligned_body = if aligned_at {
                let trimmed = body.trim_start();
                let after = trimmed.strip_prefix('{').and_then(|t| t.find('}').map(|i| &t[i + 1..])).unwrap_or(&body);
                after.to_string()
            } else {
                body.clone()
            };
            let rows = self.split_environment_rows(&aligned_body);
            let mut out: Vec<String> = Vec::new();
            for row in rows {
                let cells: Vec<&str> = row.split('&').collect();
                let source = if aligned_at {
                    let mut pairs: Vec<String> = Vec::new();
                    for idx in 0..cells.len().div_ceil(2) {
                        let a = cells.get(idx * 2).copied().unwrap_or("");
                        let b = cells.get(idx * 2 + 1).copied().unwrap_or("");
                        pairs.push(format!("{a}{b}"));
                    }
                    pairs.join(" ")
                } else {
                    cells.join("")
                };
                let rendered = self.render_nested(&source, true).trim().to_string();
                if !rendered.is_empty() {
                    out.push(rendered);
                }
            }
            return out.join("\n");
        }

        if environment == "cases" || environment == "cases*" {
            let rows = self.split_environment_rows(&body);
            let mut parsed_rows: Vec<Vec<String>> = Vec::new();
            for row in rows {
                let cells: Vec<String> = row
                    .split('&')
                    .map(|c| self.render_nested(c, false).trim().to_string())
                    .collect();
                if cells.iter().any(|c| !c.is_empty()) {
                    parsed_rows.push(cells);
                }
            }
            let mut out = Vec::new();
            for (index, row) in parsed_rows.iter().enumerate() {
                let value = row.first().cloned().unwrap_or_default();
                let value = value.trim_end_matches(',').to_string();
                let condition = row.get(1).cloned().unwrap_or_default();
                let delimiter = if index == 0 {
                    "⎧"
                } else if index == parsed_rows.len() - 1 {
                    "⎩"
                } else {
                    "⎨"
                };
                // Upstream regex /^(?:if|when|for|otherwise)\b/i — word
                // boundary so "otherwise." also matches.
                let first_word = condition
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<String>();
                let is_condition_word = matches!(
                    first_word.as_str(),
                    "if" | "when" | "for" | "otherwise" | "If" | "When" | "For" | "Otherwise"
                );
                let condition_prefix = if is_condition_word { " " } else { " if " };
                out.push(format!(
                    "{delimiter} {value}{}{condition}",
                    if condition.is_empty() { "" } else { condition_prefix }
                ));
            }
            return out.join("\n");
        }

        if matches!(
            environment.as_str(),
            "array" | "matrix" | "smallmatrix" | "pmatrix" | "bmatrix" | "Bmatrix" | "vmatrix" | "Vmatrix"
        ) {
            let matrix_body = if environment == "array" {
                let trimmed = body.trim_start();
                trimmed.strip_prefix('{').and_then(|t| t.find('}').map(|i| &t[i + 1..])).unwrap_or(&body).to_string()
            } else {
                body.clone()
            };
            return self.render_matrix(&environment, &matrix_body);
        }

        self.supported = false;
        body
    }

    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let rows = self.split_environment_rows(body);
        let matrix: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| self.render_nested(cell, false).trim().to_string())
                    .collect()
            })
            .filter(|row: &Vec<String>| row.iter().any(|c| !c.is_empty()))
            .collect();

        let column_count = matrix.iter().map(|row| row.len()).max().unwrap_or(0);
        let column_widths: Vec<usize> = (0..column_count)
            .map(|column| {
                matrix
                    .iter()
                    .map(|row| visible_width(row.get(column).map(|s| s.as_str()).unwrap_or("")))
                    .max()
                    .unwrap_or(0)
            })
            .collect();

        let rendered_rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..column_count)
                    .map(|column| {
                        let cell = row.get(column).map(|s| s.as_str()).unwrap_or("");
                        let padding = column_widths[column].saturating_sub(visible_width(cell));
                        format!("{cell}{}", PROTECTED_SPACE.repeat(padding))
                    })
                    .collect::<Vec<String>>()
                    .join(" │ ")
            })
            .collect();

        let lines: Vec<String> = if environment == "array" || environment == "matrix" || environment == "smallmatrix" {
            rendered_rows
        } else {
            let delimiters: &[&str] = match environment {
                "pmatrix" => &["⎛", "⎞", "⎜", "⎟", "⎝", "⎠"],
                "bmatrix" => &["⎡", "⎤", "⎢", "⎥", "⎣", "⎦"],
                "Bmatrix" => &["⎧", "⎫", "⎨", "⎬", "⎩", "⎭"],
                "vmatrix" => &["│", "│", "│", "│", "│", "│"],
                "Vmatrix" => &["║", "║", "║", "║", "║", "║"],
                _ => {
                    self.supported = false;
                    return rendered_rows.join("\n");
                }
            };
            rendered_rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let left = if index == 0 {
                        delimiters[0]
                    } else if index == rendered_rows.len() - 1 {
                        delimiters[4]
                    } else {
                        delimiters[2]
                    };
                    let right = if index == 0 {
                        delimiters[1]
                    } else if index == rendered_rows.len() - 1 {
                        delimiters[5]
                    } else {
                        delimiters[3]
                    };
                    format!("{left} {row} {right}")
                })
                .collect()
        };

        if lines.len() <= 1 {
            return lines.first().cloned().unwrap_or_default();
        }
        let index = self.layout_nodes.len();
        self.layout_nodes.push(LayoutNode::Matrix { lines, baseline: 0 });
        format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
    }

    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        let mut nested = LatexParser::new(source, self.layout_nodes, self.display && stack_fractions);
        let rendered = nested.render();
        if rendered.is_none() {
            self.supported = false;
            return source.to_string();
        }
        rendered.unwrap()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineLowerStyle {
    Bracket,
    Script,
}

fn is_marker_operand(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, ']' | ')' | '}' | '\u{f0001}')
}

fn is_marker_after(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '√' | '\u{f0000}')
}

/// Upstream `normalizeOutput`: insert spacing around named operators, then
/// strip the markers and trim lines.
fn normalize_output(value: &str) -> String {
    // 1) Space before NAMED_OPERATOR_START when the previous char is an operand.
    let mut s = String::with_capacity(value.len());
    let chars: Vec<char> = value.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '\u{f0004}' && i > 0 && is_marker_operand(chars[i - 1]) {
            s.push(' ');
        }
        s.push(*c);
    }
    let s = s.replace(NAMED_OPERATOR_START, "");
    // 2) Space after NAMED_OPERATOR_END when the next char is an operand.
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, c) in chars.iter().enumerate() {
        out.push(*c);
        if *c == '\u{f0005}' && i + 1 < chars.len() && is_marker_after(chars[i + 1]) {
            out.push(' ');
        }
    }
    let s = out.replace(NAMED_OPERATOR_END, "");

    let raw_lines: Vec<&str> = s.split('\n').collect();
    let count = raw_lines.len();
    let mut lines: Vec<String> = Vec::new();
    for (index, line) in raw_lines.iter().enumerate() {
        let cleaned = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let keep = !cleaned.is_empty() || (index > 0 && index < count - 1);
        if keep {
            lines.push(cleaned);
        }
    }
    lines.join("\n").trim().to_string()
}

/// Upstream `renderLatex`.
pub fn render_latex(source: &str, display: bool) -> Option<String> {
    let mut layout_nodes: Vec<LayoutNode> = Vec::new();
    let rendered = LatexParser::new(source, &mut layout_nodes, display).render();
    let rendered = rendered?;
    if layout_nodes.is_empty() {
        return Some(rendered.replace(PROTECTED_SPACE, " "));
    }
    let lines = render_layout(&rendered, &layout_nodes).lines;
    let indentation = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    Some(
        lines
            .iter()
            .map(|line| line[indentation.min(line.len())..].trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
            .replace(PROTECTED_SPACE, " "),
    )
}

#[path = "latex_tests.rs"]
#[cfg(test)]
mod latex_tests;
