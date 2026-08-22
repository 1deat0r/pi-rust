//! Session usage aggregation — port of
//! `packages/coding-agent/src/core/usage-totals.ts`.
//!
//! Aggregates per-provider/model usage and cost from session usage records.
//! The grouping exactly mirrors upstream `getUsageCostBreakdown`:
//! - assistant messages attribute to `provider/responseModel ?? provider/model`;
//! - tool-result usage and branch_summary/compaction usage bucket to
//!   `Tools/summaries`;
//! - entries with zero cost and zero tokens are filtered out;
//! - result is sorted by cost descending.

use serde_json::Value;

/// Aggregate usage counters (upstream `UsageTotals`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

pub fn create_usage_totals() -> UsageTotals {
    UsageTotals::default()
}

/// Add one `Usage` record to totals. Upstream adds `usage.cost.total`.
pub fn add_usage_to_totals(totals: &mut UsageTotals, usage: &pi_ai::types::Usage) {
    totals.input += usage.input;
    totals.output += usage.output;
    totals.cache_read += usage.cache_read;
    totals.cache_write += usage.cache_write;
    totals.cost += usage.cost.total;
}

/// One row of the cost breakdown: a group key, its total cost, and its
/// total token count (input + output + cacheRead + cacheWrite).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageCostBreakdownEntry {
    pub key: String,
    pub cost: f64,
    pub tokens: u64,
}

/// Minimal view of a session entry sufficient for usage attribution.
///
/// Parsed from the on-disk session JSONL entries (upstream `SessionEntry`).
/// Only the fields used by `getUsageCostBreakdown` are extracted.
#[derive(Debug, Clone)]
pub enum SessionEntryUsageView {
    /// A message entry with role `assistant`.
    Assistant {
        provider: String,
        model: String,
        response_model: Option<String>,
        usage: Option<pi_ai::types::Usage>,
    },
    /// A message entry with role `toolResult` carrying usage.
    ToolResult { usage: pi_ai::types::Usage },
    /// A `branch_summary` or `compaction` entry carrying usage.
    Summary { usage: pi_ai::types::Usage },
    /// Any entry that does not contribute to the breakdown.
    Other,
}

/// Parse one session JSONL entry into its usage view.
/// Unknown/malformed entries fall back to `Other`.
pub fn parse_session_entry(value: &Value) -> SessionEntryUsageView {
    let Some(entry_type) = value.get("type").and_then(|v| v.as_str()) else {
        return SessionEntryUsageView::Other;
    };
    let usage_of = |v: &Value| {
        v.get("usage")
            .and_then(|u| serde_json::from_value(u.clone()).ok())
    };
    match entry_type {
        "message" => {
            let Some(msg) = value.get("message") else {
                return SessionEntryUsageView::Other;
            };
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "assistant" {
                let provider = msg
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let model = msg
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let response_model = msg
                    .get("responseModel")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let usage = usage_of(msg);
                SessionEntryUsageView::Assistant { provider, model, response_model, usage }
            } else if role == "toolResult" {
                match usage_of(msg) {
                    Some(usage) => SessionEntryUsageView::ToolResult { usage },
                    None => SessionEntryUsageView::Other,
                }
            } else {
                SessionEntryUsageView::Other
            }
        }
        "branch_summary" | "compaction" => match usage_of(value) {
            Some(usage) => SessionEntryUsageView::Summary { usage },
            None => SessionEntryUsageView::Other,
        },
        _ => SessionEntryUsageView::Other,
    }
}

/// Compute the cost breakdown over session entries (upstream
/// `getUsageCostBreakdown`). `entries` are the parsed JSONL record values.
pub fn get_usage_cost_breakdown(entries: &[Value]) -> Vec<UsageCostBreakdownEntry> {
    let mut totals_by_key: Vec<(String, UsageTotals)> = Vec::new();

    for entry in entries {
        let (key, usage) = match parse_session_entry(entry) {
            SessionEntryUsageView::Assistant { provider, model, response_model, usage } => {
                let Some(usage) = usage else { continue };
                (format!("{provider}/{}", response_model.unwrap_or(model)), usage)
            }
            SessionEntryUsageView::ToolResult { usage } => ("Tools/summaries".to_string(), usage),
            SessionEntryUsageView::Summary { usage } => ("Tools/summaries".to_string(), usage),
            SessionEntryUsageView::Other => continue,
        };
        if let Some((_, existing)) = totals_by_key.iter_mut().find(|(k, _)| *k == key) {
            add_usage_to_totals(existing, &usage);
        } else {
            let mut new_totals = create_usage_totals();
            add_usage_to_totals(&mut new_totals, &usage);
            totals_by_key.push((key, new_totals));
        }
    }

    let mut entries: Vec<UsageCostBreakdownEntry> = totals_by_key
        .into_iter()
        .map(|(key, totals)| {
            let tokens = totals.input + totals.output + totals.cache_read + totals.cache_write;
            UsageCostBreakdownEntry { key, cost: totals.cost, tokens }
        })
        .collect();
    entries.retain(|entry| entry.cost > 0.0 || entry.tokens > 0);
    entries.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::{Cost, Usage};
    use serde_json::json;

    fn usage(input: u64, output: u64, total: f64) -> Usage {
        Usage {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + output,
            cost: Cost {
                input: if input == 0 { 0.0 } else { 1.0 },
                output: 1.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total,
            },
        }
    }


    #[test]
    fn assistant_usage_attributes_to_provider_model() {
        let entries = vec![
            json!({
                "type": "message",
                "message": {
                    "role": "assistant",
                    "provider": "google",
                    "model": "gemini-3.1-pro-preview",
                    "usage": { "input": 10, "output": 5, "cacheRead": 2, "cacheWrite": 0, "totalTokens": 17, "cost": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0.012 } }
                }
            }),
        ];
        let breakdown = get_usage_cost_breakdown(&entries);
        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].key, "google/gemini-3.1-pro-preview");
        assert_eq!(breakdown[0].tokens, 17);
        assert!((breakdown[0].cost - 0.012).abs() < 1e-9);
    }

    #[test]
    fn response_model_wins_over_model_for_key() {
        let entries = vec![
            json!({
                "type": "message",
                "message": {
                    "role": "assistant",
                    "provider": "openrouter",
                    "model": "some/router",
                    "responseModel": "provider-x/actual",
                    "usage": { "input": 1, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 1, "cost": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0.001 } }
                }
            }),
        ];
        let breakdown = get_usage_cost_breakdown(&entries);
        assert_eq!(breakdown[0].key, "openrouter/provider-x/actual");
    }

    #[test]
    fn tool_and_summary_usage_bucket_to_tools() {
        let entries = vec![
            json!({
                "type": "message",
                "message": {
                    "role": "toolResult",
                    "usage": { "input": 3, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 3, "cost": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0.004 } }
                }
            }),
            json!({
                "type": "compaction",
                "summary": "ctx",
                "retainedTail": [],
                "tokensBefore": 5,
                "usage": { "input": 2, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2, "cost": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0.003 } }
            }),
            json!({
                "type": "branch_summary",
                "fromId": "e1",
                "summary": "s",
                "usage": { "input": 1, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 1, "cost": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0.001 } }
            }),
        ];
        let breakdown = get_usage_cost_breakdown(&entries);
        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].key, "Tools/summaries");
        assert_eq!(breakdown[0].tokens, 6);
        assert!((breakdown[0].cost - 0.008).abs() < 1e-9);
    }

    #[test]
    fn zero_cost_zero_tokens_filtered() {
        let entries = vec![
            json!({
                "type": "message",
                "message": {
                    "role": "assistant",
                    "provider": "faux",
                    "model": "faux-1",
                    "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0, "cost": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0 } }
                }
            }),
        ];
        let breakdown = get_usage_cost_breakdown(&entries);
        assert!(breakdown.is_empty());
    }

    #[test]
    fn sorted_by_cost_descending() {
        let entries = vec![
            json!({
                "type": "message",
                "message": {
                    "role": "assistant",
                    "provider": "a",
                    "model": "m1",
                    "usage": { "input": 1, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 1, "cost": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0.01 } }
                }
            }),
            json!({
                "type": "message",
                "message": {
                    "role": "assistant",
                    "provider": "b",
                    "model": "m2",
                    "usage": { "input": 1, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 1, "cost": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0.05 } }
                }
            }),
        ];
        let breakdown = get_usage_cost_breakdown(&entries);
        assert_eq!(breakdown[0].key, "b/m2");
        assert_eq!(breakdown[1].key, "a/m1");
    }

    #[test]
    fn user_messages_ignored() {
        let entries = vec![
            json!({
                "type": "message",
                "message": { "role": "user", "content": "hi" }
            }),
            json!({ "type": "custom", "customType": "x", "data": {} }),
        ];
        let breakdown = get_usage_cost_breakdown(&entries);
        assert!(breakdown.is_empty());
    }

    #[test]
    fn aggregates_same_key_across_entries() {
        let e = |v: Value| v;
        let usage = usage(4, 4, 0.02);
        let mut entries = vec![json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "provider": "x",
                "model": "m",
                "usage": serde_json::to_value(&usage).unwrap()
            }
        })];
        entries.push(e(json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "provider": "x",
                "model": "m",
                "usage": serde_json::to_value(&usage).unwrap()
            }
        })));
        let breakdown = get_usage_cost_breakdown(&entries);
        assert_eq!(breakdown[0].key, "x/m");
        assert_eq!(breakdown[0].tokens, 16);
        assert!((breakdown[0].cost - 0.04).abs() < 1e-9);
    }
}
