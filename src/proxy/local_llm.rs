//! Local inference servers: llama.cpp `llama-server` timings, Ollama counters, backend probing.

use std::time::Duration;

use serde_json::Value;

use super::read_u64;
use crate::monitor::Usage;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Merge llama-server `timings` (`cache_n` reused, `prompt_n` prefilled, `prompt_ms`).
pub fn merge_llama_timings(event: &Value, acc: &mut Usage) {
    let Some(timings) = event.get("timings") else {
        return;
    };
    let cache_n = read_u64(timings, "/cache_n");
    let prompt_n = read_u64(timings, "/prompt_n");
    if cache_n.is_none() && prompt_n.is_none() {
        return;
    }
    acc.cached_tokens = cache_n.or(acc.cached_tokens);
    acc.prefilled_tokens = prompt_n.or(acc.prefilled_tokens);
    acc.prompt_tokens = Some(cache_n.unwrap_or(0) + prompt_n.unwrap_or(0));
    acc.prompt_ms = timings
        .get("prompt_ms")
        .and_then(Value::as_f64)
        .or(acc.prompt_ms);
}

/// Merge Ollama `/api/chat` counters. Ollama does not report reused tokens, so only the
/// prefill size and time are known.
pub fn merge_ollama(event: &Value, acc: &mut Usage) {
    let Some(count) = read_u64(event, "/prompt_eval_count") else {
        return;
    };
    acc.prefilled_tokens = Some(count);
    acc.prompt_tokens = Some(count);
    acc.prompt_ms = event
        .get("prompt_eval_duration")
        .and_then(Value::as_f64)
        .map(|ns| ns / 1_000_000.0);
}

/// Detect llama-server and its parallel slot count. Returns `None` for other servers.
pub async fn probe_llama_slots(client: &reqwest::Client, base: &str) -> Option<u32> {
    if let Some(props) = get_json(client, &format!("{base}/props")).await {
        let is_llama = props.get("default_generation_settings").is_some()
            || props.get("total_slots").is_some();
        if is_llama {
            let slots = read_u64(&props, "/total_slots").unwrap_or(1);
            return Some(u32::try_from(slots).unwrap_or(1));
        }
    }
    let slots = get_json(client, &format!("{base}/slots")).await?;
    slots
        .as_array()
        .map(|s| u32::try_from(s.len()).unwrap_or(1))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    let response = client.get(url).timeout(PROBE_TIMEOUT).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_llama_timings() {
        let mut usage = Usage::default();
        merge_llama_timings(
            &json!({"timings": {"cache_n": 81000, "prompt_n": 120, "prompt_ms": 150.0}}),
            &mut usage,
        );
        assert_eq!(usage.cached_tokens, Some(81000));
        assert_eq!(usage.prompt_tokens, Some(81120));
        assert_eq!(usage.prompt_ms, Some(150.0));
    }

    #[test]
    fn reads_ollama_counters() {
        let mut usage = Usage::default();
        merge_ollama(
            &json!({"done": true, "prompt_eval_count": 500,
                "prompt_eval_duration": 250_000_000u64}),
            &mut usage,
        );
        assert_eq!(usage.prefilled_tokens, Some(500));
        assert_eq!(usage.prompt_ms, Some(250.0));
        assert_eq!(usage.cached_tokens, None);
    }
}
