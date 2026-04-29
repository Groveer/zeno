//! Token usage tracking across the session.

use crate::api::types::Usage;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct CostTracker {
    total_input_tokens: u64,
    total_output_tokens: u64,
    turn_count: u64,
    model_costs: HashMap<String, ModelCost>,
}

#[derive(Debug, Default)]
struct ModelCost {
    input_tokens: u64,
    output_tokens: u64,
    calls: u64,
}

impl CostTracker {
    pub fn record(&mut self, model: &str, usage: &Usage) {
        self.total_input_tokens += usage.input_tokens;
        self.total_output_tokens += usage.output_tokens;
        self.turn_count += 1;

        let entry = self.model_costs.entry(model.to_string()).or_default();
        entry.input_tokens += usage.input_tokens;
        entry.output_tokens += usage.output_tokens;
        entry.calls += 1;
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
    }

    pub fn summary(&self) -> String {
        format!(
            "Total: {} tokens ({} in + {} out) across {} turns",
            self.total_tokens(),
            self.total_input_tokens,
            self.total_output_tokens,
            self.turn_count,
        )
    }
}
