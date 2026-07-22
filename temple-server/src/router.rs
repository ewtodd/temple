use crate::config::ModelConfig;
use temple_protocol::ComplexityClass;

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
    /// Heuristic classification — fast, no model call. Designed to be
    /// conservative: only classifies obviously-Simple and obviously-Complex.
    /// Everything else stays Medium, which gets refined by a model call in
    /// parallel with queue acquisition.
    pub fn classify(query: &str) -> ComplexityClass {
        let q = query.to_lowercase();
        let len = query.trim().len();

        // ── Obvious Simple: greetings, thanks, status, very short ──
        if len < 15
            || q == "hi"
            || q == "hey"
            || q == "hello"
            || q == "yo"
            || q == "sup"
            || q == "thanks"
            || q == "thank you"
            || q == "thx"
            || q == "ty"
            || q == "ok"
            || q == "okay"
            || q == "k"
            || q == "kk"
            || q == "status"
            || q == "help"
            || q == "ping"
            || q == "lol"
            || q == "nice"
            || q == "cool"
        {
            return ComplexityClass::Simple;
        }

        // Greeting-prefixed short messages are Simple
        if q.starts_with("hello ")
            || q.starts_with("hi ")
            || q.starts_with("hey ")
            || q.starts_with("thanks ")
            || q.starts_with("thank you ")
        {
            let rest = q.split_once(' ').map(|x| x.1).unwrap_or("");
            if rest.len() < 30 {
                return ComplexityClass::Simple;
            }
        }

        // ── Obvious Complex: coding keywords co-occurring with substance ──
        // Require BOTH a coding signal AND substantive content (not just
        // "what's an error code" type questions).
        let has_code_signal = q.contains(" code")
            || q.contains(" bug")
            || q.contains("fix ")
            || q.contains("implement ")
            || q.contains("refactor")
            || q.contains(" rewrite")
            || q.contains("rewrite ")
            || q.contains(" build")
            || q.contains("building ")
            || q.contains("compile")
            || q.contains(" debug")
            || q.contains("debugging ")
            || q.contains("commit ")
            || q.contains("push ")
            || q.contains(" test")
            || q.contains("testing ");

        let has_substance = len > 40
            || q.contains('.')
            || q.contains('/')
            || q.contains('(')
            || q.contains("src")
            || q.contains("fn ")
            || q.contains("def ")
            || q.contains("class ")
            || q.contains("struct ")
            || q.contains("import ")
            || q.contains("use ")
            || q.contains("cargo")
            || q.contains("nix ")
            || q.contains("flake")
            || q.contains("flake.")
            || q.contains(".rs")
            || q.contains(".py")
            || q.contains(".nix")
            || q.contains(".cpp")
            || q.contains(".cxx")
            || q.contains(".c ")
            || q.contains(".ts")
            || q.contains(".js")
            || q.contains(".go")
            || q.contains("mod ")
            || q.contains("trait ")
            || q.contains("impl ");

        if has_code_signal && has_substance {
            return ComplexityClass::Complex;
        }

        // ── Obvious Critical: explicit design/architecture requests ──
        if (q.contains("design ") || q.contains("architecture"))
            && len > 60
            && (q.contains("new ") || q.contains("from scratch") || q.contains("system"))
        {
            return ComplexityClass::Critical;
        }

        // ── Default: Medium (deferred to model for refinement) ──
        ComplexityClass::Medium
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
                ComplexityClass::Complex => Route::Direct {
                    model: models.executor_model.clone(),
                },
                ComplexityClass::Critical => Route::Pipeline {
                    planner: models.planner_model.clone(),
                    executor: models.executor_model.clone(),
                    reviewer: models.reviewer_model.clone(),
                },
            },
        }
    }
}
