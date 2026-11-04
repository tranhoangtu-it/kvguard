//! End-to-end tests: real proxy on a random port in front of a wiremock upstream.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use kvguard::config::{Config, Upstreams};
use kvguard::diff::MutationKind;
use kvguard::diff::jinja_verifier::JinjaVerifier;
use kvguard::lineage::MatchKind;
use kvguard::monitor::{Monitor, TurnRecord, TurnStatus};
use kvguard::provider::ApiKind;
use kvguard::proxy::{self, AppState};

const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(20);

struct Harness {
    url: String,
    monitor: Arc<Monitor>,
    upstream: MockServer,
    client: reqwest::Client,
}

impl Harness {
    async fn start(heal: bool, configure: impl FnOnce(&mut Config)) -> Self {
        let upstream = MockServer::start().await;
        let mut config = Config::new(Upstreams::resolve(Some(&upstream.uri()), |_| None));
        configure(&mut config);
        let monitor = Arc::new(Monitor::new(heal, 850.0));
        let state = AppState::new(config, Arc::clone(&monitor)).expect("state");
        let listener = proxy::bind(Some(0)).await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(proxy::serve(listener, state));
        Self {
            url,
            monitor,
            upstream,
            client: reqwest::Client::new(),
        }
    }

    async fn post(&self, route: &str, body: &Value) -> String {
        let response = self
            .client
            .post(format!("{}{route}", self.url))
            .json(body)
            .send()
            .await
            .expect("proxy reachable");
        assert!(
            response.status().is_success(),
            "status {}",
            response.status()
        );
        response.text().await.expect("body")
    }

    /// Wait until `count` turns have finished background analysis.
    async fn analyzed_turns(&self, count: usize) -> Vec<TurnRecord> {
        let deadline = tokio::time::Instant::now() + ANALYSIS_TIMEOUT;
        loop {
            let turns = self.monitor.snapshot().turns;
            let done = turns.len() >= count && turns.iter().all(|t| t.est_prompt_tokens.is_some());
            if done {
                return turns;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "analysis timed out: {turns:#?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// JSON bodies the upstream received on `route`, in order.
    async fn upstream_bodies(&self, route: &str) -> Vec<Value> {
        self.upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.url.path() == route)
            .map(|r| serde_json::from_slice(&r.body).expect("upstream body is JSON"))
            .collect()
    }
}

/// Roughly `words * 2` tokens of stable conversation context.
fn filler(words: usize) -> String {
    (0..words).map(|i| format!("ctx{} ", i % 997)).collect()
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

async fn mount_anthropic_sse(server: &MockServer) {
    let sse = "event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":9000}}}\n\n\
        event: message_delta\n\
        data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(server)
        .await;
}

fn claude_turn(cch: &str, extra: &[(&str, &str)]) -> Value {
    let mut messages = vec![
        json!({"role": "user", "content": filler(5_000)}),
        json!({"role": "assistant", "content": "I have read the repository context."}),
    ];
    messages.extend(
        extra
            .iter()
            .map(|(role, text)| json!({"role": role, "content": text})),
    );
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 1024,
        "stream": true,
        "system": format!(
            "x-anthropic-billing-header: cc_version=2.1.0; cch={cch};\nYou are Claude Code, a coding agent."
        ),
        "messages": messages,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn test_detect_and_heal_claude_code_billing_header() {
    let turn1 = claude_turn("11111", &[("user", "Explain the build.")]);
    let turn2 = claude_turn(
        "22222",
        &[
            ("user", "Explain the build."),
            ("assistant", "It uses cargo."),
            ("user", "Now add CI."),
        ],
    );

    // Inspect mode: the bust is diagnosed at the very first tokens of the system prompt.
    let inspect = Harness::start(false, |_| {}).await;
    mount_anthropic_sse(&inspect.upstream).await;
    let streamed = inspect.post("/v1/messages", &turn1).await;
    assert!(streamed.contains("message_start"), "SSE relayed unchanged");
    inspect.post("/v1/messages", &turn2).await;
    let turns = inspect.analyzed_turns(2).await;
    let bust = &turns[1];
    assert_eq!(bust.status, TurnStatus::Bust);
    let diagnosis = bust.diagnosis.as_ref().expect("diagnosis");
    assert_eq!(diagnosis.kind, MutationKind::VolatileSystemHeader);
    assert_eq!(diagnosis.json_path, "system");
    assert!(
        diagnosis.divergence_token < 40,
        "diverges inside the header"
    );
    assert!(
        diagnosis.invalidated_tokens > 9_000,
        "whole context invalidated"
    );
    assert_eq!(
        turns[0].usage.as_ref().and_then(|u| u.cached_tokens),
        Some(9_000),
        "usage parsed from the SSE stream"
    );

    // Heal mode: the header never reaches the upstream and the prefix matches.
    let heal = Harness::start(true, |_| {}).await;
    mount_anthropic_sse(&heal.upstream).await;
    heal.post("/v1/messages", &turn1).await;
    heal.post("/v1/messages", &turn2).await;
    let turns = heal.analyzed_turns(2).await;
    assert_eq!(turns[1].match_kind, MatchKind::Continuation);
    assert_eq!(turns[1].status, TurnStatus::Healed);
    assert!(turns[1].diagnosis.is_none());

    let bodies = heal.upstream_bodies("/v1/messages").await;
    assert_eq!(bodies.len(), 2);
    for body in &bodies {
        assert!(!body.to_string().contains("x-anthropic-billing-header"));
    }
    assert_eq!(bodies[0]["system"], bodies[1]["system"]);
    let first = bodies[0]["messages"].as_array().expect("messages");
    let second = bodies[1]["messages"].as_array().expect("messages");
    assert_eq!(first[..], second[..first.len()]);
}

fn tool(name: &str, schema: Value) -> Value {
    json!({"name": name, "description": format!("{name} MCP tool"), "input_schema": schema})
}

#[tokio::test(flavor = "multi_thread")]
async fn test_detect_and_heal_mcp_tool_shuffle() {
    let github = tool(
        "mcp__github__create_issue",
        json!({"type": "object", "properties": {"title": {"type": "string"},
            "body": {"type": "string"}}, "required": ["title"]}),
    );
    let github_reordered = tool(
        "mcp__github__create_issue",
        json!({"required": ["title"], "properties": {"body": {"type": "string"},
            "title": {"type": "string"}}, "type": "object"}),
    );
    let linear = tool(
        "mcp__linear__create_ticket",
        json!({"type": "object", "properties": {"team": {"type": "string"}}}),
    );
    let supabase = tool(
        "mcp__supabase__query",
        json!({"type": "object", "properties": {"sql": {"type": "string"}}}),
    );
    let history = json!([{"role": "user", "content": filler(1_500)}]);
    let turn1 = json!({"model": "claude-sonnet-4-5", "system": "S",
        "tools": [github, linear, supabase], "messages": history});
    let mut turn2 = json!({"model": "claude-sonnet-4-5", "system": "S",
        "tools": [supabase, github_reordered, linear], "messages": history});
    turn2["messages"].as_array_mut().expect("messages").extend([
        json!({"role": "assistant", "content": "ok"}),
        json!({"role": "user", "content": "next"}),
    ]);

    let inspect = Harness::start(false, |_| {}).await;
    mount_anthropic_sse(&inspect.upstream).await;
    inspect.post("/v1/messages", &turn1).await;
    inspect.post("/v1/messages", &turn2).await;
    let turns = inspect.analyzed_turns(2).await;
    let diagnosis = turns[1].diagnosis.as_ref().expect("diagnosis");
    assert_eq!(diagnosis.kind, MutationKind::ToolOrderShuffled);
    assert_eq!(diagnosis.json_path, "tools[0].name");

    let heal = Harness::start(true, |_| {}).await;
    mount_anthropic_sse(&heal.upstream).await;
    heal.post("/v1/messages", &turn1).await;
    heal.post("/v1/messages", &turn2).await;
    let turns = heal.analyzed_turns(2).await;
    assert_eq!(turns[1].match_kind, MatchKind::Continuation);

    let bodies = heal.upstream_bodies("/v1/messages").await;
    let tools1 = serde_json::to_string(&bodies[0]["tools"]).expect("tools");
    let tools2 = serde_json::to_string(&bodies[1]["tools"]).expect("tools");
    assert_eq!(tools1, tools2, "tools bytes identical after JCS");
    let names: Vec<_> = bodies[1]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        names,
        [
            "mcp__github__create_issue",
            "mcp__linear__create_ticket",
            "mcp__supabase__query"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_subagent_interleaving_no_false_positive_and_slot_pinning() {
    let harness = Harness::start(true, |_| {}).await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"total_slots": 4, "default_generation_settings": {"n_ctx": 131072}}),
        ))
        .mount(&harness.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 20000},
            "timings": {"cache_n": 19500, "prompt_n": 500, "prompt_ms": 600.0}
        })))
        .mount(&harness.upstream)
        .await;

    let main_history = vec![
        json!({"role": "system", "content": "You are the main coding agent."}),
        json!({"role": "user", "content": filler(10_000)}),
    ];
    let main1 = json!({"model": "qwen3-coder", "messages": main_history});
    let sub = json!({"model": "qwen3-coder", "messages": [
        {"role": "system", "content": "You are a file-search sub-agent. Reply with paths only."},
        {"role": "user", "content": filler(700)}
    ]});
    let mut main2_messages = main_history.clone();
    main2_messages.push(json!({"role": "assistant", "content": "Context loaded."}));
    main2_messages.push(json!({"role": "user", "content": filler(250)}));
    let main2 = json!({"model": "qwen3-coder", "messages": main2_messages});

    harness.post("/v1/chat/completions", &main1).await;
    harness.post("/v1/chat/completions", &sub).await;
    harness.post("/v1/chat/completions", &main2).await;

    let turns = harness.analyzed_turns(3).await;
    assert_eq!((turns[0].branch.as_str(), turns[0].slot), ("main", Some(0)));
    assert_eq!(
        (turns[1].branch.as_str(), turns[1].slot),
        ("sub-1", Some(1))
    );
    assert_eq!(turns[1].match_kind, MatchKind::NewBranch);
    assert_eq!((turns[2].branch.as_str(), turns[2].slot), ("main", Some(0)));
    assert_eq!(turns[2].match_kind, MatchKind::Continuation);
    assert!(
        turns[2].diagnosis.is_none(),
        "no false bust against the sub-agent"
    );
    assert_ne!(turns[2].status, TurnStatus::Bust);
    assert_eq!(
        turns[2].usage.as_ref().and_then(|u| u.cached_tokens),
        Some(19_500),
        "llama-server timings parsed"
    );

    let bodies = harness.upstream_bodies("/v1/chat/completions").await;
    let slots: Vec<_> = bodies.iter().map(|b| b["id_slot"].as_u64()).collect();
    assert_eq!(slots, [Some(0), Some(1), Some(0)]);
    assert!(bodies.iter().all(|b| b["cache_prompt"] == json!(true)));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_detect_qwen_jinja_think_tag_drift() {
    let template = fixture("qwen-think-template.jinja");
    let harness = Harness::start(false, |c| c.chat_template = Some(template.clone())).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
        .mount(&harness.upstream)
        .await;

    let parent_messages = vec![
        json!({"role": "system", "content": "You are Qwen, a coding assistant."}),
        json!({"role": "user", "content": "List the Rust files."}),
        json!({"role": "assistant", "content": "Calling ls.",
            "reasoning_content": "The user wants files; call ls.",
            "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "ls", "arguments": "{}"}}]}),
        json!({"role": "tool", "tool_call_id": "c1", "content": "a.rs b.rs"}),
    ];
    let parent = json!({"model": "qwen3.5", "messages": parent_messages});
    let mut current_messages = parent_messages.clone();
    current_messages.push(json!({"role": "assistant", "content": "Files: a.rs, b.rs",
        "reasoning_content": "Summarize."}));
    current_messages.push(json!({"role": "user", "content": "Open a.rs."}));
    let current = json!({"model": "qwen3.5", "messages": current_messages});

    harness.post("/v1/chat/completions", &parent).await;
    harness.post("/v1/chat/completions", &current).await;
    let turns = harness.analyzed_turns(2).await;

    assert_eq!(
        turns[1].match_kind,
        MatchKind::Continuation,
        "JSON history is untouched"
    );
    assert_eq!(turns[1].status, TurnStatus::Bust);
    let diagnosis = turns[1].diagnosis.as_ref().expect("diagnosis");
    assert_eq!(diagnosis.kind, MutationKind::JinjaTemplateDrift);

    let verifier = JinjaVerifier::from_path(&template).expect("template");
    let rendered = verifier
        .render(ApiKind::OpenAiChat, &parent)
        .expect("render");
    let think_at = rendered.find("<think>").expect("parent renders reasoning");
    assert_eq!(
        diagnosis.divergence_byte, think_at,
        "drift located at the dropped <think>"
    );
    assert!(diagnosis.parent_excerpt.contains("⟦<think>"));
    assert!(diagnosis.divergence_token > 0 && diagnosis.divergence_token < diagnosis.parent_tokens);
}

fn anthropic_sse(read: u64, written: u64) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":\
         {{\"input_tokens\":20,\"cache_creation_input_tokens\":{written},\
         \"cache_read_input_tokens\":{read}}}}}}}\n\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn test_partial_cache_read_on_intact_prefix_is_flagged() {
    let harness = Harness::start(false, |_| {}).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(anthropic_sse(0, 9_000), "text/event-stream"),
        )
        .up_to_n_times(1)
        .mount(&harness.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(anthropic_sse(500, 9_200), "text/event-stream"),
        )
        .mount(&harness.upstream)
        .await;

    let history = vec![json!({"role": "user", "content": filler(4_000)})];
    let turn1 = json!({"model": "claude-sonnet-4-5", "system": "S", "messages": history});
    let tool_uses: Vec<Value> = (0..11)
        .map(|i| json!({"type": "tool_use", "id": format!("t{i}"), "name": "read", "input": {}}))
        .collect();
    let tool_results: Vec<Value> = (0..11)
        .map(|i| json!({"type": "tool_result", "tool_use_id": format!("t{i}"), "content": "ok"}))
        .collect();
    let mut messages = history.clone();
    messages.push(json!({"role": "assistant", "content": tool_uses}));
    messages.push(json!({"role": "user", "content": tool_results}));
    let turn2 = json!({"model": "claude-sonnet-4-5", "system": "S", "messages": messages});

    harness.post("/v1/messages", &turn1).await;
    harness.post("/v1/messages", &turn2).await;
    let turns = harness.analyzed_turns(2).await;
    // Usage arrives with the streamed body; give the tap a moment to finalize.
    let deadline = tokio::time::Instant::now() + ANALYSIS_TIMEOUT;
    let turn = loop {
        let turns = harness.monitor.snapshot().turns;
        if turns[1].status == TurnStatus::Partial || tokio::time::Instant::now() > deadline {
            break turns[1].clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        turns[1].match_kind,
        MatchKind::Continuation,
        "prefix intact"
    );
    assert_eq!(turn.status, TurnStatus::Partial, "{turn:#?}");
    assert_eq!(turn.expected_cached_tokens, Some(9_000));
    assert!(
        turn.notes.iter().any(|n| n.contains("22 content blocks")),
        "{:?}",
        turn.notes
    );

    let metrics = reqwest::get(format!("{}/__kvguard/metrics", harness.url))
        .await
        .expect("metrics reachable")
        .text()
        .await
        .expect("metrics body");
    assert!(metrics.contains("kvguard_partials_total 1"), "{metrics}");
}

#[test]
fn test_ci_gate_exit_code() {
    let dir = fixture("history-mutation");
    let file = |n: &str| dir.join(n).display().to_string();
    let run = |args: &[String]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_kvguard"))
            .arg("check")
            .args(args)
            .output()
            .expect("run kvguard check")
    };

    let healthy = run(&[
        file("turn-1.json"),
        file("turn-2.json"),
        "--fail-on-bust".into(),
        "--min-hit-rate".into(),
        "0.95".into(),
    ]);
    assert_eq!(
        healthy.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&healthy.stdout)
    );

    let broken = run(&[
        dir.join("turn-*.json").display().to_string(),
        "--fail-on-bust".into(),
    ]);
    let report = String::from_utf8_lossy(&broken.stdout);
    assert_eq!(broken.status.code(), Some(1), "{report}");
    assert!(report.contains("history message mutated"), "{report}");
    assert!(report.contains("messages[1].content"), "{report}");
}
