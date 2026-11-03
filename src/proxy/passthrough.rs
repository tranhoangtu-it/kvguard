//! Streaming passthrough: forwards requests upstream and relays response chunks as they
//! arrive, while a tap parses usage events on the side. The response body is never buffered.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;

use super::{anthropic, local_llm, openai};
use crate::monitor::Usage;
use crate::provider::ApiKind;

/// Largest non-streaming response body kept for usage parsing (the client still gets all of it).
const MAX_PARSE_BODY: usize = 16 * 1024 * 1024;
/// Largest single SSE / NDJSON line kept for parsing.
const MAX_LINE: usize = 8 * 1024 * 1024;

/// Hop-by-hop headers that must not be forwarded (RFC 9110 §7.6.1), plus framing headers
/// that are recomputed for the new body.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// Copy request headers for the upstream call.
///
/// For analyzed LLM calls, compression is disabled so usage events can be parsed.
pub fn upstream_request_headers(incoming: &HeaderMap, analyzed: bool) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(incoming.len());
    for (name, value) in incoming {
        let lower = name.as_str();
        if HOP_BY_HOP.contains(&lower) || (analyzed && lower == "accept-encoding") {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if analyzed {
        out.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
    }
    out
}

/// Copy upstream response headers back to the client.
pub fn client_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(upstream.len());
    for (name, value) in upstream {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out.insert(
        HeaderName::from_static("x-kvguard"),
        HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// `text/event-stream`: `data: {...}` lines.
    Sse,
    /// Newline-delimited JSON (Ollama streaming).
    Ndjson,
    /// A single JSON document.
    Whole,
}

/// Incremental usage parser fed with raw response chunks.
#[derive(Debug)]
pub struct UsageCollector {
    api: ApiKind,
    framing: Framing,
    line: Vec<u8>,
    body: Vec<u8>,
    usage: Usage,
    found: bool,
}

impl UsageCollector {
    pub fn new(api: ApiKind, content_type: &str) -> Self {
        let framing = if content_type.contains("text/event-stream") {
            Framing::Sse
        } else if content_type.contains("ndjson") {
            Framing::Ndjson
        } else {
            Framing::Whole
        };
        Self {
            api,
            framing,
            line: Vec::new(),
            body: Vec::new(),
            usage: Usage::default(),
            found: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        if self.framing == Framing::Whole {
            if self.body.len() + chunk.len() <= MAX_PARSE_BODY {
                self.body.extend_from_slice(chunk);
            }
            return;
        }
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.handle_line(&line);
            } else if self.line.len() < MAX_LINE {
                self.line.push(byte);
            }
        }
    }

    fn handle_line(&mut self, raw: &[u8]) {
        let Ok(text) = std::str::from_utf8(raw) else {
            return;
        };
        let text = text.trim_end_matches('\r').trim();
        let payload = match self.framing {
            Framing::Sse => match text.strip_prefix("data:") {
                Some(data) => data.trim(),
                None => return,
            },
            Framing::Ndjson | Framing::Whole => text,
        };
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        if let Ok(event) = serde_json::from_str::<Value>(payload) {
            self.merge(&event);
        }
    }

    fn merge(&mut self, event: &Value) {
        let before = self.usage.clone();
        match self.api {
            ApiKind::AnthropicMessages => anthropic::merge_usage(event, &mut self.usage),
            ApiKind::OpenAiChat | ApiKind::OpenAiResponses => {
                openai::merge_usage(event, &mut self.usage);
                local_llm::merge_llama_timings(event, &mut self.usage);
            }
            ApiKind::OllamaChat => local_llm::merge_ollama(event, &mut self.usage),
        }
        self.found |= self.usage != before;
    }

    /// Final usage, if the response carried any.
    pub fn finish(mut self) -> Option<Usage> {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.handle_line(&line);
        }
        if self.framing == Framing::Whole && !self.body.is_empty() {
            let body = std::mem::take(&mut self.body);
            if let Ok(doc) = serde_json::from_slice::<Value>(&body) {
                self.merge(&doc);
            }
        }
        self.found.then_some(self.usage)
    }
}

type DoneCallback = Box<dyn FnOnce(Option<Usage>) + Send>;

/// Stream adapter: yields upstream chunks unchanged and reports usage once, when the stream
/// ends or the client disconnects (drop).
pub struct UsageTap<S> {
    inner: S,
    collector: Option<UsageCollector>,
    on_done: Option<DoneCallback>,
}

impl<S> UsageTap<S> {
    pub fn new(
        inner: S,
        collector: UsageCollector,
        on_done: impl FnOnce(Option<Usage>) + Send + 'static,
    ) -> Self {
        Self {
            inner,
            collector: Some(collector),
            on_done: Some(Box::new(on_done)),
        }
    }

    fn finish(&mut self) {
        if let (Some(collector), Some(done)) = (self.collector.take(), self.on_done.take()) {
            done(collector.finish());
        }
    }
}

impl<S, E> Stream for UsageTap<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = Pin::new(&mut self.inner).poll_next(cx);
        match &polled {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(collector) = self.collector.as_mut() {
                    collector.feed(chunk);
                }
            }
            Poll::Ready(None) => self.finish(),
            Poll::Ready(Some(Err(_))) | Poll::Pending => {}
        }
        polled
    }
}

impl<S> Drop for UsageTap<S> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn tap_relays_chunks_and_reports_sse_usage() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(
                b"event: message_start\ndata: {\"type\":\"message_start\",",
            )),
            Ok(Bytes::from_static(
                b"\"message\":{\"usage\":{\"input_tokens\":5,\"cache_read_input_tokens\":95}}}\n\n",
            )),
            Ok(Bytes::from_static(b"data: [DONE]\n")),
        ];
        let seen = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        let tap = UsageTap::new(
            futures_util::stream::iter(chunks),
            UsageCollector::new(ApiKind::AnthropicMessages, "text/event-stream"),
            move |usage| *sink.lock().unwrap() = usage,
        );
        let relayed: Vec<_> = tap.collect().await;
        assert_eq!(relayed.len(), 3);
        let usage = seen.lock().unwrap().clone().expect("usage");
        assert_eq!(usage.cached_tokens, Some(95));
        assert_eq!(usage.prompt_tokens, Some(100));
    }

    #[test]
    fn analyzed_requests_disable_compression() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4141"));
        let out = upstream_request_headers(&headers, true);
        assert_eq!(out.get(header::ACCEPT_ENCODING).unwrap(), "identity");
        assert!(out.get(header::HOST).is_none());
    }
}
