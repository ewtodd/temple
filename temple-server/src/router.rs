use temple_protocol::{ComplexityClass, RouterDecision};

/// Lightweight query classifier.
/// Used only when use_local_router is enabled.
/// With a warm DSv4 Flash cache this is mostly unnecessary,
/// but can save a round-trip for trivial queries.
pub struct Router;

impl Router {
    /// Classify a query by heuristic keywords.
    /// No model inference needed — fast and deterministic.
    pub fn classify(query: &str, model: &str) -> RouterDecision {
        let q = query.to_lowercase();

        // Critical: completely novel/open-ended reasoning
        if q.contains("design") || q.contains("architecture") || q.contains("novel")
            || q.contains("invent") || q.contains("create a new")
        {
            return RouterDecision {
                target_model: model.to_string(),
                target_host: "son-of-anton".into(),
                reasoning: "Requires novel reasoning or system design".into(),
                complexity: ComplexityClass::Critical,
            };
        }

        // Complex: coding with implementation
        if q.contains("write code") || q.contains("implement") || q.contains("debug")
            || q.contains("refactor") || q.contains("test for")
            || (q.contains("code") && (q.contains("function") || q.contains("class")))
        {
            return RouterDecision {
                target_model: model.to_string(),
                target_host: "son-of-anton".into(),
                reasoning: "Requires code generation or debugging".into(),
                complexity: ComplexityClass::Complex,
            };
        }

        // Medium: explanation, summarization, chat
        if q.starts_with("what") || q.starts_with("how") || q.starts_with("why")
            || q.starts_with("explain") || q.contains("summarize")
            || q.contains("tell me about")
        {
            return RouterDecision {
                target_model: model.to_string(),
                target_host: "son-of-anton".into(),
                reasoning: "Explanatory or chat query — medium complexity".into(),
                complexity: ComplexityClass::Medium,
            };
        }

        // Simple: factual, greetings, status
        if q.starts_with("hello") || q.starts_with("hi") || q.starts_with("hey")
            || q.contains("status") || q.contains("thank")
            || q.len() < 20
        {
            return RouterDecision {
                target_model: "gemma-4-12b".into(),
                target_host: "e-desktop".into(),
                reasoning: "Simple query — fast model sufficient".into(),
                complexity: ComplexityClass::Simple,
            };
        }

        // Default: send to main brain
        RouterDecision {
            target_model: model.to_string(),
            target_host: "son-of-anton".into(),
            reasoning: "Default routing to primary model".into(),
            complexity: ComplexityClass::Medium,
        }
    }
}
