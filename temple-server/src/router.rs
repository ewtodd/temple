use temple_protocol::{ComplexityClass, RouterDecision};
use crate::litellm::{ChatMessage, ChatRequest, LiteLLM};

/// Request router. Uses heuristics first, falls back to local small model
/// for ambiguous cases.
pub struct Router;

impl Router {
    /// Quick heuristic classification — no model call needed.
    pub fn classify(query: &str) -> ComplexityClass {
        let q = query.to_lowercase();
        let len = query.len();

        // Simple: greetings, thanks, status, very short
        if len < 25
            || q.starts_with("hello") || q.starts_with("hi ") || q.starts_with("hey")
            || q.starts_with("thanks") || q.starts_with("thank you")
            || q == "hi" || q == "hello" || q == "hey"
            || q.contains("how are you") || q.contains("what can you do")
            || q == "status" || q == "help"
        {
            return ComplexityClass::Simple;
        }

        // Complex: coding, debugging, implementing, designing
        if q.contains("code") || q.contains("function") || q.contains("class")
            || q.contains("bug") || q.contains("error") || q.contains("debug")
            || q.contains("implement") || q.contains("refactor")
            || q.contains("write") && (q.contains("test") || q.contains("file"))
            || q.contains("design") || q.contains("architecture")
        {
            return ComplexityClass::Complex;
        }

        // Critical: novel design, system architecture
        if q.contains("design a new") || q.contains("create a new system")
            || q.contains("from scratch")
        {
            return ComplexityClass::Critical;
        }

        // Default: medium
        ComplexityClass::Medium
    }

    /// Classify using the local small model when heuristics are uncertain.
    /// Returns None if the local model is unavailable.
    pub async fn classify_with_model(
        local: &LiteLLM,
        query: &str,
    ) -> Option<ComplexityClass> {
        let heuristic = Self::classify(query);

        // Only use the model for medium-complexity (ambiguous) queries
        if heuristic != ComplexityClass::Medium {
            return Some(heuristic);
        }

        let req = ChatRequest {
            model: "qwen3-4b-instruct".to_string(),
            messages: vec![
                ChatMessage::system(
                    "Classify this user query into exactly one word: simple, medium, complex, or critical.\n\
                     simple = greeting, thanks, status, factual\n\
                     medium = explanation, chat, summary\n\
                     complex = coding, debugging, multi-step\n\
                     critical = novel design, architecture\n\
                     Respond with ONLY the word, nothing else."
                ),
                ChatMessage::user(query),
            ],
            tools: None,
            stream: Some(false),
            stream_options: None,
            max_tokens: Some(10),
            temperature: Some(0.0),
        };

        let resp = local.chat(req).await.ok()?;
        let choice = resp.choices.first()?;
        let content = choice.message.content.as_deref()?.trim().to_lowercase();

        match content.as_str() {
            s if s.starts_with("simple") => Some(ComplexityClass::Simple),
            s if s.starts_with("complex") => Some(ComplexityClass::Complex),
            s if s.starts_with("critical") => Some(ComplexityClass::Critical),
            _ => Some(ComplexityClass::Medium),
        }
    }
}
