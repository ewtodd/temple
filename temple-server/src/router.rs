use temple_protocol::ComplexityClass;
use crate::litellm::{ChatMessage, ChatRequest, LiteLLM};
use crate::config::ModelConfig;

/// Session kind — determines system prompt tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// TUI or Signal session — full model routing + code rules
    Interactive,
    /// Cron job — minimal prompt, default model only
    Headless,
}

/// Request router. Uses heuristics first, falls back to local small model
/// for ambiguous cases. Maps complexity classes to configured fleet models.
pub struct Router;

/// Where the agent should send the request.
#[derive(Debug, Clone)]
pub enum Route {
    /// Send to this model directly, no pipeline.
    Direct { model: String },
    /// Run the planner→executor→reviewer pipeline.
    Pipeline {
        planner: String,
        executor: String,
        reviewer: String,
    },
}

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
        router_model: &str,
        query: &str,
    ) -> Option<ComplexityClass> {
        let heuristic = Self::classify(query);

        // Only use the model for medium-complexity (ambiguous) queries
        if heuristic != ComplexityClass::Medium {
            return Some(heuristic);
        }

        let req = ChatRequest {
            model: router_model.to_string(),
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

    /// Map a complexity class to a concrete route.
    pub fn route(complexity: ComplexityClass, models: &ModelConfig, kind: SessionKind) -> Route {
        match kind {
            SessionKind::Headless => Route::Direct {
                model: models.default_model.clone(),
            },
            SessionKind::Interactive => match complexity {
                ComplexityClass::Simple => Route::Direct {
                    model: models.simple_model.clone(),
                },
                ComplexityClass::Medium => Route::Direct {
                    model: models.default_model.clone(),
                },
                ComplexityClass::Complex => Route::Pipeline {
                    planner: models.planner_model.clone(),
                    executor: models.executor_model.clone(),
                    reviewer: models.reviewer_model.clone(),
                },
                ComplexityClass::Critical => Route::Direct {
                    model: models.critical_model.clone(),
                },
            },
        }
    }
}
