//! Request API families and upstream backend classification.

use serde::{Deserialize, Serialize};

/// Wire format of an intercepted LLM request, derived from the request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    /// Anthropic `/v1/messages`.
    #[value(name = "anthropic")]
    #[serde(rename = "anthropic")]
    AnthropicMessages,
    /// OpenAI-compatible `/v1/chat/completions` (OpenAI, llama-server, vLLM, LM Studio).
    #[value(name = "openai-chat")]
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    /// OpenAI `/v1/responses`.
    #[value(name = "openai-responses")]
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// Ollama native `/api/chat`.
    #[value(name = "ollama")]
    #[serde(rename = "ollama")]
    OllamaChat,
}

impl ApiKind {
    /// Requests of the same family share a prompt layout, so a conversation may move between
    /// them (e.g. Ollama's native `/api/chat` and its OpenAI-compatible endpoint share one cache).
    pub fn same_layout(self, other: Self) -> bool {
        let family = |api: Self| match api {
            Self::OpenAiChat | Self::OllamaChat => 0,
            Self::AnthropicMessages => 1,
            Self::OpenAiResponses => 2,
        };
        family(self) == family(other)
    }

    /// Classify a request path. Returns `None` for paths kvguard only passes through.
    pub fn from_path(path: &str) -> Option<Self> {
        let path = path.split('?').next().unwrap_or(path).trim_end_matches('/');
        if path.ends_with("/v1/messages") {
            Some(Self::AnthropicMessages)
        } else if path.ends_with("/chat/completions") {
            Some(Self::OpenAiChat)
        } else if path.ends_with("/responses") {
            Some(Self::OpenAiResponses)
        } else if path.ends_with("/api/chat") {
            Some(Self::OllamaChat)
        } else {
            None
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::OllamaChat => "ollama",
        }
    }

    /// Best-effort guess of the API family from a bare request body (used by `kvguard check`).
    pub fn guess_from_body(body: &serde_json::Value) -> Self {
        if body.get("input").is_some() || body.get("instructions").is_some() {
            return Self::OpenAiResponses;
        }
        if body.get("system").is_some() {
            return Self::AnthropicMessages;
        }
        let has_system_role = body
            .get("messages")
            .and_then(|m| m.as_array())
            .is_some_and(|msgs| {
                msgs.iter().any(|m| {
                    matches!(
                        m.get("role").and_then(|r| r.as_str()),
                        Some("system" | "developer")
                    )
                })
            });
        if has_system_role {
            Self::OpenAiChat
        } else {
            Self::AnthropicMessages
        }
    }
}

/// What kind of server sits behind the upstream URL. Drives which heal rules are safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// The official Anthropic API (billing header must be preserved).
    AnthropicCloud,
    /// The official OpenAI API (`api.openai.com`).
    OpenAiCloud,
    /// Any other hosted provider or gateway.
    HostedCloud,
    /// llama.cpp `llama-server` exposing `slots` parallel KV slots.
    LlamaCpp { slots: u32 },
    /// Ollama native server.
    Ollama,
    /// Any other local server (vLLM, LM Studio, ...).
    LocalGeneric,
}

impl Backend {
    pub fn is_local(self) -> bool {
        matches!(
            self,
            Self::LlamaCpp { .. } | Self::Ollama | Self::LocalGeneric
        )
    }
}

/// True when the URL points at the loopback interface or a private LAN host.
pub fn is_local_url(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host_port = rest.split('/').next().unwrap_or("");
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    host == "localhost"
        || host == "::1"
        || host == "0.0.0.0"
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.ends_with(".local")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_paths() {
        assert_eq!(
            ApiKind::from_path("/v1/messages?beta=true"),
            Some(ApiKind::AnthropicMessages)
        );
        assert_eq!(
            ApiKind::from_path("/v1/chat/completions"),
            Some(ApiKind::OpenAiChat)
        );
        assert_eq!(
            ApiKind::from_path("/v1/responses"),
            Some(ApiKind::OpenAiResponses)
        );
        assert_eq!(ApiKind::from_path("/api/chat"), Some(ApiKind::OllamaChat));
        assert_eq!(ApiKind::from_path("/v1/models"), None);
    }

    #[test]
    fn detects_local_urls() {
        assert!(is_local_url("http://127.0.0.1:8080"));
        assert!(is_local_url("http://localhost:11434/v1"));
        assert!(is_local_url("http://[::1]:8080"));
        assert!(!is_local_url("https://api.anthropic.com"));
    }
}
