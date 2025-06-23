use super::Pricing;
use crate::Usage;

pub fn calculate_cost(usage: Usage, pricing: Pricing) -> f64 {
    let input_cost = usage.input_tokens as f64 * pricing.prompt;
    let output_cost = usage.output_tokens as f64 * pricing.completion;

    input_cost + output_cost
}
