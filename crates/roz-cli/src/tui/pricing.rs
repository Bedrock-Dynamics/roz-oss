/// Per-token pricing in USD (input, output) per million tokens.
pub fn model_pricing(model: &str) -> (f64, f64) {
    match model {
        // Anthropic
        s if s.contains("opus") => (15.0, 75.0),
        s if s.contains("sonnet") => (3.0, 15.0),
        s if s.contains("haiku") => (0.25, 1.25),
        // OpenAI
        s if s.contains("gpt-4o") && !s.contains("mini") => (2.5, 10.0),
        s if s.contains("gpt-4o-mini") => (0.15, 0.6),
        s if s.contains("gpt-4") => (10.0, 30.0),
        s if s.contains("o1") => (15.0, 60.0),
        // Ollama / local (free)
        _ => (0.0, 0.0),
    }
}

/// Calculate cost in USD for a turn.
pub fn calculate_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
    let (input_rate, output_rate) = model_pricing(model);
    f64::from(input_tokens).mul_add(input_rate, f64::from(output_tokens) * output_rate) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sonnet_pricing() {
        let cost = calculate_cost("claude-sonnet-4-6", 1000, 500);
        // 1000 * 3.0/1M + 500 * 15.0/1M = 0.003 + 0.0075 = 0.0105
        assert!((cost - 0.0105).abs() < 1e-10);
    }

    #[test]
    fn gpt4o_pricing() {
        let cost = calculate_cost("gpt-4o", 1000, 500);
        assert!((cost - 0.0075).abs() < 1e-10);
    }

    #[test]
    fn ollama_is_free() {
        let cost = calculate_cost("llama3", 10000, 5000);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn unknown_model_is_free() {
        let cost = calculate_cost("custom-model", 1000, 500);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn haiku_pricing() {
        let cost = calculate_cost("claude-haiku-4-5", 1000, 500);
        // 1000 * 0.25/1M + 500 * 1.25/1M = 0.00025 + 0.000625 = 0.000875
        assert!((cost - 0.000_875).abs() < 1e-10);
    }

    #[test]
    fn opus_pricing() {
        let cost = calculate_cost("claude-opus-4-6", 1000, 500);
        // 1000 * 15.0/1M + 500 * 75.0/1M = 0.015 + 0.0375 = 0.0525
        assert!((cost - 0.0525).abs() < 1e-10);
    }
}
