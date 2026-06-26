//! Tiny OpenAI-compatible chat-completions client.
//!
//! Used for classifier-shaped inferences ("is this directory a good
//! default cwd for this project?"). Blocking HTTP via `ureq`; call
//! from a dedicated thread, not from a Bevy system.
//!
//! Config from env, resolved once per call. We target the native DeepSeek
//! API (an OpenAI-compatible endpoint) by default:
//!
//! - `LLM_BASE_URL`         default `https://api.deepseek.com/v1`
//! - `LLM_API_KEY`          fallback to `DEEPSEEK_API_KEY`, `DEEPSEEK_KEY`,
//!                          then `OPENAI_API_KEY`
//! - `LLM_MODEL`            default `deepseek-chat`
//! - `LLM_RESPONSE_FORMAT`  default `json_object` (see below)
//!
//! The structured-output story is "ask for a JSON object and validate",
//! but the two endpoints we support disagree on the `response_format`
//! field, so it is pluggable via `LLM_RESPONSE_FORMAT`:
//!
//! - `json_object` (default) — `response_format: {"type":"json_object"}`.
//!   What the native DeepSeek API and OpenAI accept. DeepSeek's docs only
//!   document this form (and require the word "json" in the prompt).
//! - `json_schema` — `response_format` as a `json_schema` with a
//!   permissive `{"type":"object"}` schema. Required by the Vercel AI
//!   Gateway, which 400s on the bare `json_object` form (and conversely
//!   native DeepSeek rejects `json_schema` as "unavailable now").
//! - `none` / `text` — omit `response_format` entirely.
//!
//! So to drive the Vercel gateway instead, set e.g.
//! `LLM_BASE_URL=https://ai-gateway.vercel.sh/v1`,
//! `LLM_MODEL=deepseek/deepseek-v4-pro`, `LLM_RESPONSE_FORMAT=json_schema`.
//!
//! In every mode the field-level schema is communicated through the
//! prompt. We deserialize the returned JSON into a typed Rust struct via
//! `serde_json::from_str`; on parse failure we surface the raw text so the
//! caller can log it.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum LlmError {
    MissingApiKey,
    Http(String),
    BadResponse(String),
    ParseJson { raw: String, err: String },
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::MissingApiKey => write!(
                f,
                "no LLM_API_KEY / DEEPSEEK_API_KEY / DEEPSEEK_KEY / \
                 OPENAI_API_KEY env var set"
            ),
            LlmError::Http(e) => write!(f, "http: {}", e),
            LlmError::BadResponse(s) => write!(f, "bad response shape: {}", s),
            LlmError::ParseJson { raw, err } => {
                write!(f, "parse model output as JSON: {} (raw: {})", err, raw)
            }
        }
    }
}

impl std::error::Error for LlmError {}

/// How to request structured (JSON) output. The native DeepSeek API and
/// the Vercel AI Gateway accept opposite `response_format` shapes, so this
/// is selectable via `LLM_RESPONSE_FORMAT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResponseFormatMode {
    /// `response_format: {"type":"json_object"}`. Native DeepSeek / OpenAI.
    #[default]
    JsonObject,
    /// `response_format` as a `json_schema` with a permissive
    /// `{"type":"object"}` schema. Required by the Vercel AI Gateway.
    JsonSchema,
    /// Omit `response_format` entirely.
    None,
}

impl ResponseFormatMode {
    /// Parse from the `LLM_RESPONSE_FORMAT` env value. Unknown values fall
    /// back to the default (`json_object`).
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "json_schema" | "json-schema" | "schema" => Self::JsonSchema,
            "none" | "text" | "off" => Self::None,
            _ => Self::JsonObject,
        }
    }

    /// Build the `response_format` body field for this mode, or `None` when
    /// it should be omitted from the request entirely.
    fn body(self) -> Option<ResponseFormat> {
        match self {
            Self::JsonObject => Some(ResponseFormat::JsonObject),
            Self::JsonSchema => Some(ResponseFormat::JsonSchema {
                json_schema: JsonSchema {
                    name: "response",
                    schema: serde_json::json!({ "type": "object" }),
                },
            }),
            Self::None => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub response_format: ResponseFormatMode,
}

impl LlmConfig {
    /// Resolve config from environment. Returns `MissingApiKey` if no
    /// usable key is found in any of the recognised env vars.
    pub fn from_env() -> Result<Self, LlmError> {
        let base_url = std::env::var("LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com/v1".into());
        let api_key = std::env::var("LLM_API_KEY")
            .ok()
            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
            .or_else(|| std::env::var("DEEPSEEK_KEY").ok())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .ok_or(LlmError::MissingApiKey)?;
        let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
        let response_format = std::env::var("LLM_RESPONSE_FORMAT")
            .map(|s| ResponseFormatMode::parse(&s))
            .unwrap_or_default();
        Ok(Self {
            base_url,
            api_key,
            model,
            response_format,
        })
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    /// Omitted from the request when the configured mode is
    /// [`ResponseFormatMode::None`].
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    temperature: f32,
}

/// Turn a `ureq` send error into an `LlmError::Http` that *includes* the
/// server's response body. `ureq`'s `Error::Status` carries the full
/// `Response`, whose body holds the gateway's JSON explanation (e.g. why a
/// request 400s — bad model id, unsupported field, …). `Display` alone only
/// gives "status code 400", so we read the body out explicitly.
fn http_err(e: ureq::Error) -> LlmError {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp
                .into_string()
                .unwrap_or_else(|e| format!("<failed to read body: {}>", e));
            LlmError::Http(format!("status {}: {}", code, body))
        }
        ureq::Error::Transport(t) => LlmError::Http(t.to_string()),
    }
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// The `response_format` request field, in whichever shape the configured
/// [`ResponseFormatMode`] selects. Serializes with an internal `type` tag,
/// matching the OpenAI chat-completions schema.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseFormat {
    /// `{"type":"json_object"}` — native DeepSeek / OpenAI.
    JsonObject,
    /// `{"type":"json_schema","json_schema":{...}}` — Vercel AI Gateway.
    JsonSchema { json_schema: JsonSchema },
}

#[derive(Serialize)]
struct JsonSchema {
    name: &'static str,
    schema: serde_json::Value,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

/// One-shot classifier call. Returns `T` parsed from the model's JSON
/// response. `system` describes the role; `user` is the case under
/// classification. The schema is communicated to the model purely
/// through the prompt — the OpenAI-compat servers we target don't all
/// support strict-schema mode, so we keep things portable.
pub fn classify<T: DeserializeOwned>(
    cfg: &LlmConfig,
    system: &str,
    user: &str,
) -> Result<T, LlmError> {
    // Low temperature for classifier-shaped outputs.
    complete_json(cfg, system, user, 0.0)
}

/// Same JSON-object call shape as [`classify`] but with a caller-chosen
/// temperature, for generative uses (e.g. `style-muse` proposing theme
/// genomes) where determinism is exactly wrong.
pub fn complete_json<T: DeserializeOwned>(
    cfg: &LlmConfig,
    system: &str,
    user: &str,
    temperature: f32,
) -> Result<T, LlmError> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model: &cfg.model,
        messages: vec![
            ChatMessage {
                role: "system",
                content: system,
            },
            ChatMessage {
                role: "user",
                content: user,
            },
        ],
        response_format: cfg.response_format.body(),
        temperature,
    };
    let res = ureq::post(&url)
        // Hard cap: a hung connection must surface as an error the caller
        // can show, not block its thread (the Style Lab's "New batch"
        // once sat busy forever on a stalled call).
        .timeout(std::time::Duration::from_secs(60))
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("Content-Type", "application/json")
        .send_json(serde_json::to_value(&body).expect("serialize request"));
    let res = res.map_err(http_err)?;
    let parsed: ChatResponse = res
        .into_json()
        .map_err(|e| LlmError::BadResponse(e.to_string()))?;
    let raw = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::BadResponse("no choices in response".into()))?
        .message
        .content;
    serde_json::from_str::<T>(&raw).map_err(|e| LlmError::ParseJson {
        raw,
        err: e.to_string(),
    })
}

/// One message in a multi-turn conversation (for agent loops).
#[derive(Clone)]
pub struct Msg {
    pub role: String,
    pub content: String,
}

impl Msg {
    pub fn system(s: impl Into<String>) -> Self {
        Self { role: "system".into(), content: s.into() }
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self { role: "user".into(), content: s.into() }
    }
    pub fn assistant(s: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: s.into() }
    }
}

/// Multi-turn chat completion. Unlike [`complete_json`] (system+user only)
/// this carries a full message history — the primitive an agent loop calls
/// each step. Uses the configured `response_format` (see
/// [`ResponseFormatMode`]), so the assistant's reply is expected to be a
/// single JSON object; we return it raw for the caller to parse (an agent
/// turn is small and bespoke). Blocking HTTP —
/// call from a worker thread, never a Bevy system.
pub fn chat_json(cfg: &LlmConfig, messages: &[Msg], temperature: f32) -> Result<String, LlmError> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let msgs: Vec<ChatMessage> = messages
        .iter()
        .map(|m| ChatMessage { role: &m.role, content: &m.content })
        .collect();
    let body = ChatRequest {
        model: &cfg.model,
        messages: msgs,
        response_format: cfg.response_format.body(),
        temperature,
    };
    let res = ureq::post(&url)
        // Agent turns can involve more reasoning than a classifier; give the
        // model more room than the 60s classifier cap, but still bounded so
        // a hung connection surfaces as an error.
        .timeout(std::time::Duration::from_secs(120))
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("Content-Type", "application/json")
        .send_json(serde_json::to_value(&body).expect("serialize request"));
    let res = res.map_err(http_err)?;
    let parsed: ChatResponse = res
        .into_json()
        .map_err(|e| LlmError::BadResponse(e.to_string()))?;
    parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::BadResponse("no choices in response".into()))
        .map(|c| c.message.content)
}
