//! Fuzzy matching utilities — port of `packages/tui/src/fuzzy.ts`.
//!
//! Matches if all query characters appear in order (not necessarily
//! consecutive). Lower score = better match.

/// A fuzzy match result: whether it matched, and the score (lower is better).
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

fn is_word_boundary_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '-' | '_' | '.' | '/' | ':')
}

fn match_query(query_lower: &str, text_lower: &str) -> FuzzyMatch {
    if query_lower.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query_lower.chars().count() > text_lower.chars().count() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    let qchars: Vec<char> = query_lower.chars().collect();
    let tchars: Vec<char> = text_lower.chars().collect();

    let mut query_index = 0usize;
    let mut score = 0.0f64;
    let mut last_match_index: isize = -1;
    let mut consecutive_matches = 0usize;

    for (i, &tc) in tchars.iter().enumerate() {
        if query_index >= qchars.len() {
            break;
        }
        if tc != qchars[query_index] {
            continue;
        }
        let is_word_boundary = i == 0 || is_word_boundary_separator(tchars[i - 1]);

        // Reward consecutive matches
        if last_match_index == i as isize - 1 {
            consecutive_matches += 1;
            score -= (consecutive_matches * 5) as f64;
        } else {
            consecutive_matches = 0;
            // Penalize gaps
            if last_match_index >= 0 {
                score += (i as f64 - last_match_index as f64 - 1.0) * 2.0;
            }
        }

        // Reward word boundary matches
        if is_word_boundary {
            score -= 10.0;
        }

        // Slight penalty for later matches
        score += i as f64 * 0.1;

        last_match_index = i as isize;
        query_index += 1;
    }

    if query_index < qchars.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    if query_lower == text_lower {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

fn split_alphanumeric(query_lower: &str) -> Option<(String, String)> {
    // /^(?<letters>[a-z]+)(?<digits>[0-9]+)$/
    let chars: Vec<char> = query_lower.chars().collect();
    if chars.is_empty() {
        return None;
    }
    // Find the first digit after a run of letters.
    let letters_end = chars.iter().position(|c| c.is_ascii_digit())?;
    if letters_end == 0 {
        return None;
    }
    if !chars[..letters_end].iter().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    if !chars[letters_end..].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let letters: String = chars[..letters_end].iter().collect();
    let digits: String = chars[letters_end..].iter().collect();
    Some((letters, digits))
}

fn split_numeric_alpha(query_lower: &str) -> Option<(String, String)> {
    let chars: Vec<char> = query_lower.chars().collect();
    if chars.is_empty() {
        return None;
    }
    // First letter after a run of digits.
    let digits_end = chars.iter().position(|c| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    if !chars[..digits_end].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !chars[digits_end..].iter().all(|c| c.is_ascii_lowercase()) {
        return None;
    }
    let digits: String = chars[..digits_end].iter().collect();
    let letters: String = chars[digits_end..].iter().collect();
    Some((letters, digits))
}

/// Upstream `fuzzyMatch` — does the query match the text (case-insensitive,
/// in order)? Lower score = better match.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower = query.to_lowercase();
    let text_lower = text.to_lowercase();

    let primary = match_query(&query_lower, &text_lower);
    if primary.matches {
        return primary;
    }

    // Alpha-numeric token swap: "codex52" also matches "52codex" (with a
    // small score penalty).
    let swapped = if let Some((letters, digits)) = split_alphanumeric(&query_lower) {
        Some(format!("{digits}{letters}"))
    } else if let Some((letters, digits)) = split_numeric_alpha(&query_lower) {
        Some(format!("{digits}{letters}"))
    } else {
        None
    };

    let swapped = match swapped {
        Some(s) => s,
        None => return primary,
    };
    // Only swap when the two halves actually differ.
    if swapped == query_lower {
        return primary;
    }

    let swapped_match = match_query(&swapped, &text_lower);
    if !swapped_match.matches {
        return primary;
    }

    FuzzyMatch {
        matches: true,
        score: swapped_match.score + 5.0,
    }
}

/// Upstream `fuzzyFilter` — filter items by fuzzy quality, best first.
/// Supports whitespace- and slash-separated tokens: all tokens must match.
pub fn fuzzy_filter<T>(items: Vec<T>, query: &str, get_text: impl Fn(&T) -> String) -> Vec<T> {
    if query.trim().is_empty() {
        return items;
    }

    let tokens: Vec<String> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();

    if tokens.is_empty() {
        return items;
    }

    let mut results: Vec<(T, f64)> = Vec::new();

    for item in items {
        let text = get_text(&item);
        let mut total_score = 0.0f64;
        let mut all_match = true;
        for token in &tokens {
            let m = fuzzy_match(token, &text);
            if m.matches {
                total_score += m.score;
            } else {
                all_match = false;
                break;
            }
        }
        if all_match {
            results.push((item, total_score));
        }
    }

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_with_score_zero() {
        let r = fuzzy_match("", "anything");
        assert!(r.matches);
        assert_eq!(r.score, 0.0);
    }

    #[test]
    fn query_longer_than_text_does_not_match() {
        let r = fuzzy_match("longquery", "short");
        assert!(!r.matches);
    }

    #[test]
    fn exact_match_has_good_score() {
        let r = fuzzy_match("test", "test");
        assert!(r.matches);
        assert!(r.score < 0.0);
    }

    #[test]
    fn characters_must_appear_in_order() {
        assert!(fuzzy_match("abc", "aXbXc").matches);
        assert!(!fuzzy_match("abc", "cba").matches);
    }

    #[test]
    fn case_insensitive_matching() {
        assert!(fuzzy_match("ABC", "abc").matches);
        assert!(fuzzy_match("abc", "ABC").matches);
    }

    #[test]
    fn consecutive_matches_score_better_than_scattered() {
        let consecutive = fuzzy_match("foo", "foobar");
        let scattered = fuzzy_match("foo", "f_o_o_bar");
        assert!(consecutive.matches);
        assert!(scattered.matches);
        assert!(consecutive.score < scattered.score);
    }

    #[test]
    fn word_boundary_matches_score_better() {
        let at_boundary = fuzzy_match("fb", "foo-bar");
        let not_at_boundary = fuzzy_match("fb", "afbx");
        assert!(at_boundary.matches);
        assert!(not_at_boundary.matches);
        assert!(at_boundary.score < not_at_boundary.score);
    }

    #[test]
    fn matches_swapped_alpha_numeric_tokens() {
        let r = fuzzy_match("codex52", "gpt-5.2-codex");
        assert!(r.matches);
    }

    #[test]
    fn filter_empty_query_returns_all() {
        let items = vec![
            "apple".to_string(),
            "banana".to_string(),
            "cherry".to_string(),
        ];
        let result = fuzzy_filter(items.clone(), "", |x| x.clone());
        assert_eq!(result, items);
    }

    #[test]
    fn filter_removes_non_matching() {
        let items = vec![
            "apple".to_string(),
            "banana".to_string(),
            "cherry".to_string(),
        ];
        let result = fuzzy_filter(items, "an", |x| x.clone());
        assert!(result.contains(&"banana".to_string()));
        assert!(!result.contains(&"apple".to_string()));
        assert!(!result.contains(&"cherry".to_string()));
    }

    #[test]
    fn filter_sorts_by_match_quality() {
        let items = vec![
            "a_p_p".to_string(),
            "app".to_string(),
            "application".to_string(),
        ];
        let result = fuzzy_filter(items, "app", |x| x.clone());
        assert_eq!(result[0], "app");
    }

    #[test]
    fn filter_prioritizes_exact_matches() {
        let items = vec!["clone".to_string(), "cl".to_string()];
        let result = fuzzy_filter(items, "cl", |x| x.clone());
        assert_eq!(result, vec!["cl".to_string(), "clone".to_string()]);
    }

    #[test]
    fn filter_matches_slash_separated_provider_model_queries() {
        let item = ("gpt-5.5".to_string(), "openai-codex".to_string());
        let result = fuzzy_filter(
            vec![item.clone()],
            "openai-codex/gpt-5.5",
            |(id, provider)| format!("{id} {provider}"),
        );
        assert_eq!(result, vec![item]);
    }
}
