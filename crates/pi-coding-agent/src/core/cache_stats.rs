//! Prompt-cache waste accounting — port of
//! `packages/coding-agent/src/core/cache-stats.ts`.
//!
//! Detects prompt-cache misses across a session's assistant messages: prompt
//! tokens that were present in the previous turn's prompt (and therefore
//! should have been cache reads) but were re-billed as input/cache-write.
//! Consumed when re-deriving transcript cache notices on resume/rebuild, and
//! by cache-waste reporting. The calculation is deterministic over the
//! on-disk session entries (the same `&[Value]` shape `usage_totals` uses).

use serde_json::Value;

pub use pi_ai::types::Usage;

/// Prompt-cache TTL: idle gaps longer than this are worth mentioning as the
/// likely cause of a miss. Anthropic's default cache TTL is 5 minutes.
pub const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Per-turn misses at or below this are cache breakpoint granularity noise.
const NOISE_FLOOR_TOKENS: u64 = 1024;

/// A counted cache miss on a single assistant message (upstream `CacheMiss`).
#[derive(Debug, Clone, PartialEq)]
pub struct CacheMiss {
    /// Prompt tokens that were in the previous turn's prompt but not read
    /// from cache.
    pub missed_tokens: u64,
    /// Extra dollars paid vs. a full cache hit; 0 when pricing is unknown.
    pub missed_cost: f64,
    /// Milliseconds since the previous request (which last refreshed cache).
    pub idle_ms: u64,
    /// True when the model changed relative to the previous request.
    pub model_changed: bool,
}

/// Cumulative cache waste (upstream `CacheWasteTotals`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: u64,
    pub missed_cost: f64,
    /// Number of counted misses (turns above the noise floor).
    pub miss_count: u64,
}

/// Minimal pricing lookup (upstream `ModelPriceSource`): the per-million-token
/// cache-read price for a provider/model. `0.0` means "unknown" (yields a
/// missed-cost of 0).
pub trait ModelPriceSource {
    fn cache_read_cost_per_million(&self, provider: &str, model: &str) -> f64;
}

/// A price source that returns unknown prices for everything.
pub struct NoPrices;

impl ModelPriceSource for NoPrices {
    fn cache_read_cost_per_million(&self, _provider: &str, _model: &str) -> f64 {
        0.0
    }
}

/// A price source backed by a plain function pointer
/// (`cache_read_cost_per_million(provider, model) -> f64`).
impl ModelPriceSource for fn(&str, &str) -> f64 {
    fn cache_read_cost_per_million(&self, provider: &str, model: &str) -> f64 {
        (self)(provider, model)
    }
}

/// The last assistant request seen by a scan; everything in its prompt should
/// be cached (upstream `PreviousRequest`).
#[derive(Debug, Clone)]
struct PreviousRequest {
    prompt_tokens: u64,
    model_key: String,
    timestamp: u64,
    /// Sticky: some earlier request in this scan segment reported cache
    /// activity. Distinguishes a total miss on a cache-read-only provider
    /// (OpenAI-style) from a provider that never reports caching at all.
    reported_cache: bool,
}

/// A parsed view of an assistant message sufficient for cache accounting.
#[derive(Debug, Clone)]
pub struct AssistantView {
    pub provider: String,
    pub model: String,
    pub timestamp: u64,
    pub usage: Option<Usage>,
}

/// Parse one session entry into an assistant-message view. Returns `None`
/// for non-assistant / non-message entries. The timestamps follow the on-disk
/// JSONL shape: a message entry carries its `timestamp` at the entry level
/// (matching `pi-agent`'s `Entry::Message` decode).
pub fn parse_assistant_message(value: &Value) -> Option<AssistantView> {
    if value.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    let msg = value.get("message")?;
    if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return None;
    }
    let provider = msg.get("provider").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let model = msg.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let timestamp = value.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
    let usage = msg
        .get("usage")
        .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok());
    Some(AssistantView { provider, model, timestamp, usage })
}

fn compute_prompt_tokens(usage: &Usage) -> u64 {
    usage.input + usage.cache_read + usage.cache_write
}

fn as_previous_request(msg: &AssistantView, reported_cache: bool) -> Option<PreviousRequest> {
    let usage = msg.usage.as_ref()?;
    let prompt_tokens = compute_prompt_tokens(usage);
    if prompt_tokens == 0 {
        return None;
    }
    Some(PreviousRequest {
        prompt_tokens,
        model_key: format!("{}/{}", msg.provider, msg.model),
        timestamp: msg.timestamp,
        reported_cache: reported_cache || usage.cache_read + usage.cache_write > 0,
    })
}

/// Compute the cache miss for one assistant message relative to the previous
/// request. Returns `None` when nothing is counted: first turn, after a
/// reset, no cache activity ever reported (a provider without cache support),
/// or miss below the noise floor.
fn detect_miss(
    prev: &Option<PreviousRequest>,
    msg: &AssistantView,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let usage = msg.usage.as_ref()?;
    let prompt_tokens = compute_prompt_tokens(usage);
    // A zero-cache turn only counts when cache activity was reported before:
    // on cache-read-only providers that is a total miss, while on providers
    // that never report caching it means nothing.
    let prev = prev.as_ref()?;
    if prompt_tokens == 0 || (usage.cache_read + usage.cache_write == 0 && !prev.reported_cache) {
        return None;
    }

    let missed_tokens = prev.prompt_tokens.min(prompt_tokens).saturating_sub(usage.cache_read);
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    // Extra cost = missed tokens billed at the actual paid rate (input +
    // cacheWrite, incl. write premium) instead of the cache-read rate. Missed
    // tokens can only land in input/cacheWrite buckets, so the paid rate comes
    // straight from this message's own cost breakdown.
    let paid_tokens = usage.input + usage.cache_write;
    let paid_per_token = if paid_tokens > 0 {
        (usage.cost.input + usage.cost.cache_write) / paid_tokens as f64
    } else {
        0.0
    };
    let read_per_token = if usage.cache_read > 0 {
        usage.cost.cache_read / usage.cache_read as f64
    } else {
        models.cache_read_cost_per_million(&msg.provider, &msg.model) / 1_000_000.0
    };

    Some(CacheMiss {
        missed_tokens,
        missed_cost: missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0),
        idle_ms: msg.timestamp.saturating_sub(prev.timestamp),
        model_changed: format!("{}/{}", msg.provider, msg.model) != prev.model_key,
    })
}

struct ScanResult {
    prev: Option<PreviousRequest>,
    totals: CacheWasteTotals,
    misses: Vec<(usize, CacheMiss)>,
}

fn scan(
    entries: &[Value],
    models: &dyn ModelPriceSource,
) -> ScanResult {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();
    let mut misses: Vec<(usize, CacheMiss)> = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(entry_type, "compaction" | "branch_summary") {
            // The context legitimately changed; the next turn's prompt is new
            // content, not re-billed content. Model switches are NOT exempt:
            // they re-bill the full prompt and should be counted.
            prev = None;
            continue;
        }
        if entry_type == "message" {
            if let Some(msg) = parse_assistant_message(entry) {
                if let Some(miss) = detect_miss(&prev, &msg, models) {
                    totals.missed_tokens += miss.missed_tokens;
                    totals.missed_cost += miss.missed_cost;
                    totals.miss_count += 1;
                    misses.push((index, miss));
                }
                prev = as_previous_request(&msg, prev.as_ref().map(|p| p.reported_cache).unwrap_or(false)).or(prev);
            }
        }
    }
    ScanResult { prev, totals, misses }
}

/// Cumulative cache waste across a session: prompt tokens that should have
/// been cache reads (they were in the previous turn's prompt) but were
/// re-billed (upstream `computeCacheWaste`).
pub fn compute_cache_waste(entries: &[Value], models: &dyn ModelPriceSource) -> CacheWasteTotals {
    scan(entries, models).totals
}

/// All counted cache misses across a session, as `(entry_index, miss)` pairs
/// for the entries that paid for them. Used to re-derive transcript notices
/// when rebuilding the chat from entries (upstream `collectCacheMisses`, whose
/// map keys are the assistant message objects — keyed here by entry index).
pub fn collect_cache_misses(entries: &[Value], models: &dyn ModelPriceSource) -> Vec<(usize, CacheMiss)> {
    scan(entries, models).misses
}

/// Detect a cache miss on a just-completed assistant message. `entries` must
/// not yet contain `message` (message_end fires before persistence).
pub fn detect_cache_miss(
    entries: &[Value],
    msg: &AssistantView,
    models: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    detect_miss(&scan(entries, models).prev, msg, models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(
        input: u64,
        cache_read: u64,
        cache_write: u64,
        timestamp: u64,
    ) -> Value {
        json!({
            "type": "message",
            "timestamp": timestamp,
            "message": {
                "role": "assistant",
                "provider": "anthropic",
                "model": "claude-3",
                "usage": {
                    "input": input,
                    "output": 1,
                    "cacheRead": cache_read,
                    "cacheWrite": cache_write,
                    "totalTokens": input + cache_read + cache_write + 1,
                    "cost": {
                        "input": input as f64 * 3.0,
                        "output": 1.0,
                        "cache_read": cache_read as f64 * 0.3,
                        "cache_write": cache_write as f64 * 3.75,
                        "total": 0.0
                    }
                }
            }
        })
    }

    fn compaction(timestamp: u64) -> Value {
        json!({
            "type": "compaction",
            "timestamp": timestamp,
            "summary": "ctx",
            "retainedTail": [],
        })
    }

    #[test]
    fn no_miss_on_first_turn_without_previous() {
        let entries = vec![assistant(5000, 0, 0, 1000)];
        let totals = compute_cache_waste(&entries, &NoPrices);
        assert_eq!(totals.miss_count, 0);
    }

    #[test]
    fn counts_miss_when_previous_prompt_not_cached() {
        // Turn 1: 5000 prompt tokens, nothing cached. Turn 2: same 5000 prompt
        // tokens, but only 1000 read from cache -> 4000 should have been.
        let entries = vec![
            assistant(5000, 0, 0, 1000),
            assistant(4000, 1000, 0, 2000),
        ];
        let totals = compute_cache_waste(&entries, &NoPrices);
        assert_eq!(totals.miss_count, 1);
        assert_eq!(totals.missed_tokens, 4000);
        // paid rate = input only (3.0), read rate: from this message usage
        // (cacheRead=1000 @ 0.3) -> 4000 * (3.0 - 0.3) = 10800.
        assert!((totals.missed_cost - 10800.0).abs() < 1e-6);
    }

    #[test]
    fn noise_floor_ignores_small_misses() {
        // Only 900 tokens missed, below the 1024 noise floor.
        let entries = vec![
            assistant(5000, 0, 0, 1000),
            assistant(1000, 100, 0, 2000),
        ];
        // missed = min(5000, 1000) - 100 = 900 <= 1024 -> no count.
        let totals = compute_cache_waste(&entries, &NoPrices);
        assert_eq!(totals.miss_count, 0);
    }

    #[test]
    fn compaction_resets_previous_request() {
        // Turn 1 sets prev. A compaction resets it, so turn 3 is treated as a
        // first turn and is not counted as a miss.
        let entries = vec![
            assistant(5000, 0, 0, 1000),
            compaction(1500),
            assistant(5000, 5000, 0, 2000),
        ];
        let totals = compute_cache_waste(&entries, &NoPrices);
        assert_eq!(totals.miss_count, 0);
    }

    #[test]
    fn model_change_is_flagged_on_a_counted_miss() {
        let entries = vec![
            json!({
                "type": "message",
                "timestamp": 1000,
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "claude-3",
                    "usage": {
                        "input": 5000, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 5001,
                        "cost": { "input": 15000.0, "output": 1.0, "cache_read": 0.0, "cache_write": 0.0, "total": 0.0 }
                    }
                }
            }),
            json!({
                "type": "message",
                "timestamp": 2000,
                "message": {
                    "role": "assistant",
                    "provider": "anthropic",
                    "model": "op-4",
                    "usage": {
                        "input": 4000, "output": 1, "cacheRead": 1000, "cacheWrite": 0,
                        "totalTokens": 5001,
                        "cost": { "input": 12000.0, "output": 1.0, "cache_read": 300.0, "cache_write": 0.0, "total": 0.0 }
                    }
                }
            }),
        ];
        let misses = collect_cache_misses(&entries, &NoPrices);
        assert_eq!(misses.len(), 1);
        assert!(misses[0].1.model_changed);
        assert_eq!(misses[0].1.idle_ms, 1000);
    }

    #[test]
    fn header_price_used_when_cache_read_cost_unknown_in_message() {
        // cacheRead=0 so per-token read rate falls back to the price source.
        let entries = vec![
            assistant(5000, 0, 0, 1000),
            assistant(4000, 0, 0, 2000),
        ];
        let prices: fn(&str, &str) -> f64 = |_p, _m| 0.0_f64;
        let totals = compute_cache_waste(&entries, &prices);
        // Only counted if cache activity was reported before. First turn
        // reported no cache, so `reported_cache` is false and the zero-cache
        // turn 2 is not a miss. To exercise the price source, first turn must
        // report cacheWrite activity.
        assert_eq!(totals.miss_count, 0);

        let entries = vec![
            assistant(5000, 0, 5000, 1000), // cacheWrite reports caching
            json!({
                "type": "message",
                "timestamp": 2000,
                "message": {
                    "role": "assistant",
                    "provider": "p",
                    "model": "m",
                    "usage": {
                        "input": 3000, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                        "totalTokens": 3001,
                        "cost": { "input": 9000.0, "output": 1.0, "cache_read": 0.0, "cache_write": 0.0, "total": 0.0 }
                    }
                }
            }),
        ];
        let prices: fn(&str, &str) -> f64 = |_p, _m| 1_000_000.0_f64; // $1/M
        let totals = compute_cache_waste(&entries, &prices);
        // missed = min(10000, 3000) - 0 = 3000 > floor. paid rate = 3.0, read
        // rate = 1_000_000/1_000_000 = 1.0 -> missed_cost = 3000 * (3-1) = 6000.
        assert_eq!(totals.miss_count, 1);
        assert_eq!(totals.missed_tokens, 3000);
        assert!((totals.missed_cost - 6000.0).abs() < 1e-6);
    }

    #[test]
    fn detect_cache_miss_on_pending_message() {
        let entries = vec![assistant(5000, 0, 0, 1000)];
        let pending = AssistantView {
            provider: "anthropic".to_string(),
            model: "claude-3".to_string(),
            timestamp: 2000,
            usage: Some(Usage {
                input: 4000,
                output: 1,
                cache_read: 1000,
                cache_write: 0,
                cache_write_1h: None,
                reasoning: None,
                total_tokens: 5001,
                cost: pi_ai::types::Cost {
                    input: 12000.0,
                    output: 1.0,
                    cache_read: 300.0,
                    cache_write: 0.0,
                    total: 0.0,
                },
            }),
        };
        let miss = detect_cache_miss(&entries, &pending, &NoPrices).expect("miss");
        assert_eq!(miss.missed_tokens, 4000);
    }
}