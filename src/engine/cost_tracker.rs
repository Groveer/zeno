//! Token usage tracking across the session.
//!
//! # Design: all fields accumulate
//!
//! Unlike the old design (which kept only the *last* `input_tokens` to avoid
//! double-counting), we now **accumulate every field unconditionally**.
//!
//! ## Why accumulate?
//!
//! The user wants to know "how many tokens did the session consume in total",
//! which is the sum of every API call's billable tokens — the same number that
//! appears in provider dashboards.  Each API response reports the **full
//! request's** token count (including the conversation history re-sent with
//! that request).  While the *context window pressure* at any moment is
//! `last_input_tokens + last_output_tokens`, the *cumulative consumption*
//! is the sum across all calls.
//!
//! Example — a 3-turn conversation:
//!
//! | Turn | API-reported input | API-reported output | Cumulative total |
//! |------|-------------------|--------------------|------------------|
//! | 1    | 1,000             | 100                | 1,100            |
//! | 2    | 2,100             | 200                | 3,300            |
//! | 3    | 3,300             | 300                | 6,900            |
//!
//! Naively using the *last* input (3,300) + accumulated output (600) = 3,900
//! would under-report by 43%.  The accumulated total (6,900) matches the
//! actual cost basis.
//!
//! ## Cache and reasoning tokens
//!
//! Providers return separate cache and reasoning token counts in their
//! usage objects.  These are tracked in dedicated fields so they can be
//! displayed individually and factored into cost estimates.

use crate::api::types::Usage;

/// Per-model token usage breakdown.
#[derive(Debug, Default, Clone)]
pub struct ModelCost {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub calls: u64,
}

/// Session-level token usage tracker.
///
/// All fields are monotonically increasing accumulators, except
/// `last_prompt_tokens` which tracks the most recent API call's
/// prompt-side size for context-window pressure display (matching
/// hermes-agent's `context_compressor.last_prompt_tokens`).
#[derive(Debug, Default)]
pub struct CostTracker {
    /// Accumulated input (non-cached) tokens across all API calls.
    pub total_input_tokens: u64,
    /// Accumulated output tokens across all API calls.
    pub total_output_tokens: u64,
    /// Accumulated cache-read tokens across all API calls.
    pub total_cache_read_input_tokens: u64,
    /// Accumulated cache-creation tokens across all API calls.
    pub total_cache_creation_input_tokens: u64,
    /// Accumulated reasoning tokens across all API calls.
    pub total_reasoning_tokens: u64,
    /// Number of API calls recorded.
    pub turn_count: u64,
    /// Per-model breakdown.
    model_costs: std::collections::HashMap<String, ModelCost>,
    /// Prompt-side tokens from the **last** API response (includes cache).
    ///
    /// This is `input_tokens + cache_read + cache_write` — equivalent to
    /// the API's `prompt_tokens` / `input_tokens` (Anthropic) total.
    /// Used by the status bar to show `used / context_max` like hermes-agent.
    pub last_prompt_tokens: u64,
    /// Output tokens from the **last** API response.
    ///
    /// Combined with `last_prompt_tokens`, this represents the full context
    /// window pressure at the end of the most recent API call.
    pub last_output_tokens: u64,
}

impl CostTracker {
    /// Record a single API response's usage.
    ///
    /// All token fields are **added** to the running totals.
    pub fn record(&mut self, model: &str, usage: &Usage) {
        self.total_input_tokens += usage.input_tokens;
        self.total_output_tokens += usage.output_tokens;
        self.total_cache_read_input_tokens += usage.cache_read_input_tokens;
        self.total_cache_creation_input_tokens += usage.cache_creation_input_tokens;
        self.total_reasoning_tokens += usage.reasoning_tokens;
        self.turn_count += 1;

        // Track last-call context pressure (for status bar "used / ctx" display)
        self.last_prompt_tokens = usage.prompt_tokens();
        self.last_output_tokens = usage.output_tokens;

        let entry = self.model_costs.entry(model.to_string()).or_default();
        entry.input_tokens += usage.input_tokens;
        entry.output_tokens += usage.output_tokens;
        entry.cache_read_input_tokens += usage.cache_read_input_tokens;
        entry.cache_creation_input_tokens += usage.cache_creation_input_tokens;
        entry.reasoning_tokens += usage.reasoning_tokens;
        entry.calls += 1;
    }

    /// Grand total of all token categories.
    ///
    /// Matches the provider dashboard: sum of prompt_tokens + completion_tokens.
    /// `reasoning_tokens` is NOT added separately — it is already included
    /// within `output_tokens` (the raw completion_tokens from the API).
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens
            + self.total_output_tokens
            + self.total_cache_read_input_tokens
            + self.total_cache_creation_input_tokens
    }

    /// Total cached tokens (read + write).
    pub fn total_cached_tokens(&self) -> u64 {
        self.total_cache_read_input_tokens + self.total_cache_creation_input_tokens
    }

    /// Per-model breakdown (for display / `/cost` command).
    pub fn model_breakdown(&self) -> Vec<(String, ModelCost)> {
        let mut entries: Vec<_> = self
            .model_costs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by(|a, b| b.1.calls.cmp(&a.1.calls));
        entries
    }

    /// Absorb sub-agent token usage into this tracker.
    /// Adds all token counts from the sub-agent's result without incrementing
    /// turn_count (sub-agent turns are not parent turns).
    pub fn absorb_subagent(&mut self, model: &str, input_tokens: u64, output_tokens: u64) {
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        let entry = self.model_costs.entry(model.to_string()).or_default();
        entry.input_tokens += input_tokens;
        entry.output_tokens += output_tokens;
        entry.calls += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[test]
    fn test_accumulates_input_tokens() {
        let mut ct = CostTracker::default();
        ct.record("model-a", &make_usage(1000, 100));
        ct.record("model-a", &make_usage(2100, 200));
        ct.record("model-a", &make_usage(3300, 300));

        // All input tokens accumulate: 1000 + 2100 + 3300 = 6400
        assert_eq!(ct.total_input_tokens, 6400);
        // All output tokens accumulate: 100 + 200 + 300 = 600
        assert_eq!(ct.total_output_tokens, 600);
        // Grand total: 6400 + 600 = 7000
        assert_eq!(ct.total_tokens(), 7000);
        assert_eq!(ct.turn_count, 3);
    }

    #[test]
    fn test_cache_and_reasoning_tokens() {
        let mut ct = CostTracker::default();
        ct.record(
            "model-a",
            &Usage {
                input_tokens: 1000,
                output_tokens: 200,
                cache_read_input_tokens: 500,
                cache_creation_input_tokens: 100,
                reasoning_tokens: 50,
            },
        );
        assert_eq!(ct.total_input_tokens, 1000);
        assert_eq!(ct.total_output_tokens, 200);
        assert_eq!(ct.total_cache_read_input_tokens, 500);
        assert_eq!(ct.total_cache_creation_input_tokens, 100);
        assert_eq!(ct.total_reasoning_tokens, 50);
        // total = 1000 (input) + 200 (output, includes reason) + 500 + 100 = 1800
        assert_eq!(ct.total_tokens(), 1800);
        // cached = 500 + 100 = 600
        assert_eq!(ct.total_cached_tokens(), 600);
    }

    #[test]
    fn test_model_breakdown() {
        let mut ct = CostTracker::default();
        ct.record("model-a", &make_usage(100, 10));
        ct.record("model-b", &make_usage(200, 20));
        ct.record("model-a", &make_usage(300, 30));

        let breakdown = ct.model_breakdown();
        assert_eq!(breakdown.len(), 2);
        // model-a has 2 calls, model-b has 1 → model-a first
        assert_eq!(breakdown[0].0, "model-a");
        assert_eq!(breakdown[0].1.calls, 2);
        assert_eq!(breakdown[0].1.input_tokens, 400);
        assert_eq!(breakdown[0].1.output_tokens, 40);
    }
}
