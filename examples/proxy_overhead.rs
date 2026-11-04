//! Measures the latency kvguard adds in front of a streaming LLM endpoint.
//!
//! A local mock upstream answers `/v1/messages` with an SSE stream immediately; requests carry
//! a ~80K-token Anthropic-style conversation that grows every turn. Direct and proxied requests
//! are interleaved, and time-to-first-byte (TTFB) plus total time are reported.
//!
//! ```text
//! cargo run --release --example proxy_overhead            # 300 turns per mode
//! cargo run --release --example proxy_overhead -- 1000    # custom turn count
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Value, json};

use kvguard::config::{Config, Upstreams};
use kvguard::monitor::Monitor;
use kvguard::proxy::{self, AppState};

const WARMUP: usize = 20;
/// ~80K tokens of conversation history (roughly 4 bytes per token).
const HISTORY_BYTES: usize = 320_000;
const SSE_CHUNKS: usize = 40;

async fn mock_upstream() -> String {
    // Consume the request body like a real server; replying early resets the connection.
    async fn messages(_request: Bytes) -> Response {
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":\
            {\"input_tokens\":12,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":80000}}}\n\n";
        let chunks = std::iter::once(Ok::<_, std::io::Error>(Bytes::from(start))).chain(
            (0..SSE_CHUNKS).map(|i| {
                Ok(Bytes::from(format!(
                    "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"tok{i} \"}}}}\n\n"
                )))
            }),
        );
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(futures_util::stream::iter(chunks)))
            .expect("response")
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/messages", post(messages)),
        )
        .await
    });
    url
}

async fn start_proxy(upstream: &str, heal: bool) -> String {
    let config = Config::new(Upstreams::resolve(Some(upstream), |_| None));
    let monitor = Arc::new(Monitor::new(heal, 850.0));
    let state = AppState::new(config, monitor).expect("state");
    let listener = proxy::bind(Some(0)).await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(proxy::serve(listener, state));
    url
}

fn conversation(turn: usize) -> Value {
    let filler: String =
        "The repository contains a Rust proxy that inspects prompts. ".repeat(HISTORY_BYTES / 60);
    let mut messages = vec![json!({"role": "user", "content": filler})];
    for i in 0..turn {
        messages.push(json!({"role": "assistant", "content": format!("answer {i}")}));
        messages.push(json!({"role": "user", "content": format!("question {i}")}));
    }
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 64,
        "stream": true,
        "system": [{"type": "text", "text": "You are a coding agent."}],
        "tools": (0..30).map(|i| json!({"name": format!("tool_{i}"),
            "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}))
            .collect::<Vec<_>>(),
        "messages": messages,
    })
}

/// (time to first byte, total time) for one streamed request.
async fn timed(client: &reqwest::Client, url: &str, body: &[u8]) -> (Duration, Duration) {
    let start = Instant::now();
    let response = client
        .post(format!("{url}/v1/messages"))
        .header("content-type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("request");
    let mut stream = response.bytes_stream();
    let first = stream.next().await;
    let ttfb = start.elapsed();
    assert!(first.is_some(), "empty response");
    while stream.next().await.is_some() {}
    (ttfb, start.elapsed())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn summarize(label: &str, mut samples: Vec<f64>) -> [f64; 3] {
    samples.sort_by(f64::total_cmp);
    let stats = [
        percentile(&samples, 0.50),
        percentile(&samples, 0.95),
        percentile(&samples, 0.99),
    ];
    println!(
        "{label:<26} p50 {:>7.2} ms   p95 {:>7.2} ms   p99 {:>7.2} ms",
        stats[0], stats[1], stats[2]
    );
    stats
}

#[tokio::main]
async fn main() {
    let turns: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(300);
    let upstream = mock_upstream().await;
    let inspect = start_proxy(&upstream, false).await;
    let heal = start_proxy(&upstream, true).await;
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(4)
        .build()
        .expect("client");

    let bodies: Vec<Vec<u8>> = (0..turns + WARMUP)
        .map(|t| serde_json::to_vec(&conversation(t % 50)).expect("json"))
        .collect();
    println!(
        "kvguard proxy overhead · {turns} turns per mode · request body ≈ {} KB (~80K tokens) · {SSE_CHUNKS}-chunk SSE response\n",
        bodies[0].len() / 1024
    );

    let mut ttfb = [Vec::new(), Vec::new(), Vec::new()];
    let mut total = [Vec::new(), Vec::new(), Vec::new()];
    for (i, body) in bodies.iter().enumerate() {
        for (mode, url) in [&upstream, &inspect, &heal].into_iter().enumerate() {
            let (first, all) = timed(&client, url, body).await;
            if i >= WARMUP {
                ttfb[mode].push(first.as_secs_f64() * 1000.0);
                total[mode].push(all.as_secs_f64() * 1000.0);
            }
        }
    }

    println!("Time to first byte");
    let direct = summarize("  direct", ttfb[0].clone());
    let inspect_ttfb = summarize("  via kvguard (inspect)", ttfb[1].clone());
    let heal_ttfb = summarize("  via kvguard (--heal)", ttfb[2].clone());
    println!("\nTotal response time");
    summarize("  direct", total[0].clone());
    summarize("  via kvguard (inspect)", total[1].clone());
    summarize("  via kvguard (--heal)", total[2].clone());
    println!(
        "\nAdded TTFB at p50: inspect +{:.2} ms, heal +{:.2} ms",
        inspect_ttfb[0] - direct[0],
        heal_ttfb[0] - direct[0]
    );
}
