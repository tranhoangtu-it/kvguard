//! Local HTTP/SSE reverse proxy.
//!
//! Inline (on the request path): optional heal rules → block hashing → lineage match →
//! routing hints → forward. Off the request path: token counting and diagnosis.

pub mod anthropic;
pub mod local_llm;
pub mod openai;
pub mod passthrough;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::canonicalizer::{Healer, apply_routing_rules};
use crate::config::{BackendHint, Config};
use crate::diff::{analyze, jinja_verifier::JinjaVerifier};
use crate::lineage::{MatchOutcome, SessionTree, Snapshot};
use crate::monitor::{Monitor, TurnRecord};
use crate::provider::{ApiKind, Backend, is_local_url};
use passthrough::{UsageCollector, UsageTap};

/// Default listen port; when busy the OS assigns a free one.
pub const DEFAULT_PORT: u16 = 4141;
/// Path prefix of kvguard's own control endpoints.
pub const CONTROL_PREFIX: &str = "/__kvguard";
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A failed backend probe is retried after this long (the server may still be starting).
const NEGATIVE_PROBE_TTL: Duration = Duration::from_secs(60);

/// Only loopback `Host` values are served, which defeats DNS-rebinding from web pages.
fn is_loopback_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return true;
    };
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    matches!(
        name.trim_start_matches('[').trim_end_matches(']'),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// Control endpoints additionally refuse browser requests (any `Origin`), blocking CSRF.
fn is_trusted_control_request(headers: &HeaderMap) -> bool {
    is_loopback_host(headers) && !headers.contains_key(header::ORIGIN)
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "kvguard: loopback clients only").into_response()
}

/// Classify a non-local upstream by host.
fn cloud_backend(base: &str) -> Backend {
    if base.contains("api.anthropic.com") {
        Backend::AnthropicCloud
    } else if base.contains("api.openai.com") {
        Backend::OpenAiCloud
    } else {
        Backend::HostedCloud
    }
}

pub(crate) fn read_u64(value: &Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer).and_then(Value::as_u64)
}

/// Shared proxy state.
pub struct AppState {
    pub config: Config,
    pub monitor: Arc<Monitor>,
    healer: Healer,
    tree: Mutex<SessionTree>,
    client: reqwest::Client,
    /// Base URL → detected backend, with an expiry for negative probe results.
    backends: Mutex<HashMap<String, (Backend, Option<Instant>)>>,
    jinja: Option<Arc<JinjaVerifier>>,
    recorder: Option<Mutex<File>>,
    record_failed: AtomicBool,
}

/// Request body to forward, plus the turn id when the request is an analyzed LLM call.
struct Prepared {
    body: Bytes,
    turn: Option<(u64, ApiKind)>,
}

impl Prepared {
    fn passthrough(body: Bytes) -> Self {
        Self { body, turn: None }
    }
}

impl AppState {
    pub fn new(config: Config, monitor: Arc<Monitor>) -> Result<Arc<Self>> {
        let jinja = config
            .chat_template
            .as_deref()
            .map(JinjaVerifier::from_path)
            .transpose()?
            .map(Arc::new);
        let recorder = config
            .record
            .as_deref()
            .map(|path| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("failed to open record file {}", path.display()))
            })
            .transpose()?
            .map(Mutex::new);
        // A proxy must hand redirects back to the client, never follow them with its credentials.
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build HTTP client")?;
        Ok(Arc::new(Self {
            config,
            monitor,
            healer: Healer::new(),
            tree: Mutex::new(SessionTree::new()),
            client,
            backends: Mutex::new(HashMap::new()),
            jinja,
            recorder,
            record_failed: AtomicBool::new(false),
        }))
    }

    /// Classify the upstream once per base URL (and size the llama-server slot pool).
    async fn backend_for(&self, base: &str, api: ApiKind) -> Backend {
        if api == ApiKind::OllamaChat {
            return Backend::Ollama;
        }
        let cached = self.backends.lock().ok().and_then(|cache| {
            cache
                .get(base)
                .filter(|(_, expiry)| expiry.is_none_or(|at| Instant::now() < at))
                .map(|(backend, _)| *backend)
        });
        if let Some(backend) = cached {
            return backend;
        }
        // Probe without holding any lock so a slow upstream never blocks other requests.
        let (detected, probed_negative) = self.detect_backend(base).await;
        if let Backend::LlamaCpp { slots } = detected
            && let Ok(mut tree) = self.tree.lock()
        {
            tree.set_slots(slots);
        }
        let expiry = probed_negative.then(|| Instant::now() + NEGATIVE_PROBE_TTL);
        if let Ok(mut cache) = self.backends.lock() {
            cache.insert(base.to_string(), (detected, expiry));
        }
        detected
    }

    /// Returns the backend and whether it came from a failed probe (cached only briefly).
    async fn detect_backend(&self, base: &str) -> (Backend, bool) {
        let local = is_local_url(base);
        let overridden = self.config.slots_override;
        match self.config.backend_hint {
            BackendHint::LlamaCpp => match overridden {
                Some(slots) => (Backend::LlamaCpp { slots }, false),
                None => match local_llm::probe_llama_slots(&self.client, base).await {
                    Some(slots) => (Backend::LlamaCpp { slots }, false),
                    None => (Backend::LlamaCpp { slots: 1 }, true),
                },
            },
            BackendHint::Ollama => (Backend::Ollama, false),
            BackendHint::Generic if local => (Backend::LocalGeneric, false),
            BackendHint::Generic | BackendHint::Auto if !local => (cloud_backend(base), false),
            BackendHint::Generic | BackendHint::Auto => {
                match local_llm::probe_llama_slots(&self.client, base).await {
                    Some(slots) => (
                        Backend::LlamaCpp {
                            slots: overridden.unwrap_or(slots),
                        },
                        false,
                    ),
                    None => (Backend::LocalGeneric, true),
                }
            }
        }
    }

    /// Heal, match lineage and register the turn. Falls back to passthrough for non-JSON bodies.
    async fn prepare(&self, api: ApiKind, path: &str, base: &str, raw: Bytes) -> Prepared {
        let mut value = match serde_json::from_slice::<Value>(&raw) {
            Ok(v) if v.is_object() => v,
            _ => return Prepared::passthrough(raw),
        };
        let backend = self.backend_for(base, api).await;
        let heal = self.monitor.heal_enabled();
        let mut actions = if heal {
            self.healer
                .apply_prefix_rules(api, backend, &mut value, &self.config.heal_options)
        } else {
            Vec::new()
        };

        let snapshot = Snapshot::new(api, Arc::new(value.clone()));
        let Ok(outcome) = self.tree.lock().map(|mut t| t.observe(snapshot.clone())) else {
            return Prepared::passthrough(raw);
        };
        if heal {
            actions.extend(apply_routing_rules(api, backend, &mut value, outcome.slot));
        }
        let body = if actions.is_empty() {
            raw
        } else {
            serde_json::to_vec(&value).map(Bytes::from).unwrap_or(raw)
        };
        self.record(path, &value);

        let mut record = TurnRecord::new(
            api,
            snapshot.model.clone(),
            outcome.branch_label.clone(),
            outcome.kind,
        );
        record.slot = outcome.slot.filter(|_| heal);
        record.local = backend.is_local();
        record.heal_actions = actions;
        let id = self.monitor.add_turn(record);
        self.spawn_analysis(id, outcome, snapshot);
        Prepared {
            body,
            turn: Some((id, api)),
        }
    }

    fn spawn_analysis(&self, id: u64, outcome: MatchOutcome, current: Snapshot) {
        let monitor = Arc::clone(&self.monitor);
        let jinja = self.jinja.clone();
        tokio::task::spawn_blocking(move || {
            let result = analyze(outcome.parent.as_ref(), &current, jinja.as_deref());
            monitor.update(id, |turn| {
                turn.diagnosis = result.diagnosis;
                turn.est_prompt_tokens = Some(result.est_prompt_tokens);
                turn.est_reused_tokens = Some(result.est_reused_tokens);
            });
        });
    }

    /// Append the forwarded request to the `--record` JSONL file.
    fn record(&self, path: &str, request: &Value) {
        let Some(recorder) = &self.recorder else {
            return;
        };
        let line = json!({"path": path, "request": request});
        let written = recorder
            .lock()
            .map_err(|e| e.to_string())
            .and_then(|mut file| {
                serde_json::to_writer(&mut *file, &line)
                    .map_err(|e| e.to_string())
                    .and_then(|()| file.write_all(b"\n").map_err(|e| e.to_string()))
            });
        if let Err(err) = written
            && !self.record_failed.swap(true, Ordering::Relaxed)
        {
            eprintln!("kvguard: recording disabled after write error: {err}");
        }
    }
}

/// Build the axum router (control endpoints + catch-all proxy).
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(&format!("{CONTROL_PREFIX}/state"), get(get_state))
        .route(&format!("{CONTROL_PREFIX}/heal"), post(set_heal))
        .fallback(proxy)
        .with_state(state)
}

/// Bind on loopback: the requested port, or 4141 with an OS-assigned fallback.
pub async fn bind(port: Option<u16>) -> Result<TcpListener> {
    match port {
        Some(port) => TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("port {port} is not available")),
        None => match TcpListener::bind(("127.0.0.1", DEFAULT_PORT)).await {
            Ok(listener) => Ok(listener),
            Err(_) => TcpListener::bind(("127.0.0.1", 0))
                .await
                .context("failed to bind a local port"),
        },
    }
}

/// Serve until the listener fails or the task is dropped.
pub async fn serve(listener: TcpListener, state: Arc<AppState>) -> Result<()> {
    let addr = listener.local_addr()?;
    state.monitor.set_listen(format!("http://{addr}"));
    axum::serve(listener, router(state))
        .await
        .context("proxy server stopped")
}

async fn get_state(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_trusted_control_request(&headers) {
        return forbidden();
    }
    axum::Json(state.monitor.snapshot()).into_response()
}

/// `POST /__kvguard/heal` with `{"enabled": bool}`, or an empty body to toggle.
async fn set_heal(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if !is_trusted_control_request(&headers) {
        return forbidden();
    }
    let requested = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("enabled").and_then(Value::as_bool));
    let enabled = match requested {
        Some(enabled) => {
            state.monitor.set_heal(enabled);
            enabled
        }
        None => state.monitor.toggle_heal(),
    };
    axum::Json(json!({"heal_enabled": enabled})).into_response()
}

async fn proxy(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if !is_loopback_host(request.headers()) {
        return forbidden();
    }
    match forward(&state, request).await {
        Ok(response) => response,
        Err(err) => {
            let body = json!({"error": {
                "type": "kvguard_upstream_error",
                "message": format!("{err:#}"),
            }});
            (StatusCode::BAD_GATEWAY, axum::Json(body)).into_response()
        }
    }
}

async fn forward(state: &Arc<AppState>, request: Request) -> Result<Response> {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_string();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| path.clone(), |pq| pq.as_str().to_string());
    let base = state.config.upstreams.for_path(&path).to_string();
    let url = format!("{base}{path_and_query}");
    let raw = axum::body::to_bytes(body, MAX_REQUEST_BODY)
        .await
        .context("request body unreadable or larger than 64 MiB")?;

    let api = (parts.method == Method::POST)
        .then(|| ApiKind::from_path(&path))
        .flatten();
    let prepared = match api {
        Some(api) => state.prepare(api, &path, &base, raw).await,
        None => Prepared::passthrough(raw),
    };

    let headers = passthrough::upstream_request_headers(&parts.headers, prepared.turn.is_some());
    let sent = state
        .client
        .request(parts.method.clone(), &url)
        .headers(headers)
        .body(prepared.body)
        .send()
        .await;
    let upstream = match sent {
        Ok(response) => response,
        Err(err) => {
            if let Some((id, _)) = prepared.turn {
                let message = format!("upstream unreachable: {err}");
                state.monitor.update(id, |t| t.error = Some(message));
            }
            return Err(err).with_context(|| format!("upstream {base} unreachable"));
        }
    };

    let status = upstream.status();
    let headers = passthrough::client_response_headers(upstream.headers());
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let stream = Box::pin(upstream.bytes_stream());
    let body = match prepared.turn {
        Some((id, api)) => {
            state.monitor.update(id, |t| {
                t.http_status = Some(status.as_u16());
                if !status.is_success() {
                    t.error = Some(format!("upstream HTTP {status}"));
                }
            });
            let monitor = Arc::clone(&state.monitor);
            let collector = UsageCollector::new(api, &content_type);
            let tap = UsageTap::new(stream, collector, move |usage| {
                if let Some(usage) = usage {
                    monitor.update(id, |t| t.usage = Some(usage));
                }
            });
            Body::from_stream(tap)
        }
        None => Body::from_stream(stream),
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        if let Some(origin) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        h
    }

    #[test]
    fn only_loopback_hosts_are_served() {
        assert!(is_loopback_host(&headers("127.0.0.1:4141", None)));
        assert!(is_loopback_host(&headers("localhost", None)));
        assert!(is_loopback_host(&headers("[::1]:4141", None)));
        assert!(!is_loopback_host(&headers("evil.example:4141", None)));
    }

    #[test]
    fn control_endpoints_reject_browser_origins() {
        assert!(is_trusted_control_request(&headers("127.0.0.1:4141", None)));
        assert!(!is_trusted_control_request(&headers(
            "127.0.0.1:4141",
            Some("https://evil.example")
        )));
    }

    #[test]
    fn cloud_hosts_are_classified() {
        assert_eq!(
            cloud_backend("https://api.anthropic.com"),
            Backend::AnthropicCloud
        );
        assert_eq!(
            cloud_backend("https://api.openai.com"),
            Backend::OpenAiCloud
        );
        assert_eq!(
            cloud_backend("https://openrouter.ai/api"),
            Backend::HostedCloud
        );
    }
}
