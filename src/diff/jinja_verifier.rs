//! Renders chat templates to catch KV-cache drift that only appears after templating
//! (e.g. Qwen templates dropping `<think>` from earlier assistant turns).

use std::path::Path;

use anyhow::{Context, Result, bail};
use minijinja::{Environment, Error, ErrorKind, context};
use serde_json::Value;

use crate::canonicalizer::sanitizer::conversation_key;
use crate::provider::ApiKind;

const TEMPLATE_NAME: &str = "chat";

/// Built-in ChatML template (Qwen 2.x / generic) used when no template file is given.
pub const CHATML_TEMPLATE: &str = "{%- for message in messages %}<|im_start|>{{ message.role }}\n\
{{ message.content }}<|im_end|>\n{% endfor %}\
{%- if add_generation_prompt %}<|im_start|>assistant\n{% endif %}";

/// Divergence found in rendered template output.
#[derive(Debug, Clone)]
pub struct JinjaDrift {
    pub divergence_byte: usize,
    pub parent_rendered: String,
    pub current_rendered: String,
}

/// A loaded chat template.
pub struct JinjaVerifier {
    env: Environment<'static>,
}

impl std::fmt::Debug for JinjaVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JinjaVerifier")
    }
}

impl JinjaVerifier {
    /// Build from template source.
    pub fn from_source(source: impl Into<String>) -> Result<Self> {
        let mut env = Environment::new();
        env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
        env.add_function("raise_exception", raise_exception);
        env.add_function("strftime_now", strftime_now);
        env.add_template_owned(TEMPLATE_NAME, source.into())
            .context("invalid chat template")?;
        Ok(Self { env })
    }

    /// Load a raw `.jinja` file or the `chat_template` field of a `tokenizer_config.json`.
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read chat template {}", path.display()))?;
        let is_json = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
        let source = if is_json {
            template_from_tokenizer_config(&raw)
                .with_context(|| format!("no chat_template in {}", path.display()))?
        } else {
            raw
        };
        Self::from_source(source)
    }

    /// Render the conversation the way the inference server would (generation prompt on).
    pub fn render(&self, api: ApiKind, body: &Value) -> Result<String> {
        let messages = body
            .get(conversation_key(api))
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        let tools = body.get("tools").cloned();
        let template = self.env.get_template(TEMPLATE_NAME)?;
        let rendered = template.render(context! {
            messages => minijinja::Value::from(minijinja::value::Serde(&messages)),
            tools => minijinja::Value::from(minijinja::value::Serde(&tools)),
            add_generation_prompt => true,
            bos_token => "",
            eos_token => "",
        })?;
        Ok(rendered)
    }

    /// The parent's rendered prompt must be a prefix of the current one for the KV cache to
    /// be reused. Returns the drift when it is not.
    pub fn check(
        &self,
        api: ApiKind,
        parent: &Value,
        current: &Value,
    ) -> Result<Option<JinjaDrift>> {
        let parent_rendered = self.render(api, parent)?;
        let current_rendered = self.render(api, current)?;
        if current_rendered.starts_with(&parent_rendered) {
            return Ok(None);
        }
        let divergence_byte =
            super::token_diff::common_prefix_bytes(&parent_rendered, &current_rendered);
        Ok(Some(JinjaDrift {
            divergence_byte,
            parent_rendered,
            current_rendered,
        }))
    }
}

fn template_from_tokenizer_config(raw: &str) -> Result<String> {
    let config: Value = serde_json::from_str(raw).context("invalid JSON")?;
    match config.get("chat_template") {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(named)) => named
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some("default"))
            .or_else(|| named.first())
            .and_then(|t| t.get("template"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("chat_template list is empty"),
        _ => bail!("missing chat_template"),
    }
}

fn raise_exception(message: String) -> Result<String, Error> {
    Err(Error::new(ErrorKind::InvalidOperation, message))
}

fn strftime_now(format: String) -> String {
    chrono::Local::now().format(&format).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chatml_continuation_has_no_drift() {
        let v = JinjaVerifier::from_source(CHATML_TEMPLATE).expect("template");
        let parent = json!({"messages": [{"role": "user", "content": "hi"}]});
        let current = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "next"}
        ]});
        let drift = v
            .check(ApiKind::OpenAiChat, &parent, &current)
            .expect("render");
        assert!(drift.is_none());
    }

    #[test]
    fn reads_named_template_list() {
        let raw = r#"{"chat_template": [{"name": "tool_use", "template": "x"},
            {"name": "default", "template": "y"}]}"#;
        assert_eq!(template_from_tokenizer_config(raw).unwrap(), "y");
    }
}
