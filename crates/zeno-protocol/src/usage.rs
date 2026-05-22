//! Token usage tracking types.

use serde::{Deserialize, Serialize};

/// Token usage from a single API call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Some providers report cache hit tokens separately.
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    /// Total tokens consumed (input + output).
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Merge another usage record into this one (accumulates).
    pub fn merge(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }
}

impl std::ops::Add for Usage {
    type Output = Self;
    fn add(mut self, rhs: Self) -> Self {
        self.merge(&rhs);
        self
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.merge(&rhs);
    }
}
