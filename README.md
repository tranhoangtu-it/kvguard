# kvguard

**Local proxy & TUI that catches 1-token prompt/KV cache prefix misses in real time — and can heal them before the request leaves your machine.**

Works with Anthropic (`/v1/messages`), OpenAI (`/v1/chat/completions`, `/v1/responses`), llama.cpp `llama-server`, Ollama, and any OpenAI-compatible local server. One static Rust binary. No Docker, no database, no account.

```bash
kvguard --heal -- claude                                  # Claude Code against the Anthropic API
kvguard --heal --target http://127.0.0.1:8080 -- opencode # any agent against llama-server
kvguard check recorded.jsonl --fail-on-bust               # CI gate
```

## Why

Prompt caches (Anthropic, OpenAI) and local KV caches (llama.cpp, vLLM, Ollama) only reuse work when the new prompt starts with **exactly** the same bytes as a previous one, in order `tools → system → messages`. One changed token near the top — a rotating header, a timestamp, MCP tools serialized in a different order, a rewritten history message, a chat template that renders an old turn differently — and everything after it is billed or prefilled again. Nothing errors. You just pay more and wait longer.

kvguard sits between your agent and the model and, for every request:

1. **Finds the conversation it continues** — sub-agents, title generators and parallel sessions get their own branch, so they are never reported as busts of the main session.
2. **Diffs it against the previous turn of that branch** and, if the prefix broke, reports *where* (JSON path + token index) and *why*.
3. **Optionally rewrites it** (`--heal`) so the prefix stays byte-stable.
4. **Reads real cache usage** from the response stream (`cache_read_input_tokens`, `cached_tokens`, llama.cpp `timings.cache_n`) without buffering it.

## What it detects

| Cause | Typical source | Healable |
|---|---|---|
| `tools[] order shuffled` | MCP servers starting asynchronously | ✅ sort by name |
| `tool schema key order changed` | non-deterministic JSON Schema serialization | ✅ RFC 8785 key ordering |
| `volatile header/id in system` | Claude Code `x-anthropic-billing-header: cch=…` against non-Anthropic backends, request ids | ✅ strip (local / third-party backends only) |
| `timestamp in prefix` | `Current time: …` with seconds in the system prompt | ✅ pin per session |
| `history message mutated` | context compaction, placeholder injection, truncated tool output | ❌ reported with path |
| `chat template drift` | Qwen/DeepSeek templates dropping `<think>` from earlier turns | ✅ `--normalize-think-tags` |
| `system prompt changed`, `tool set changed`, `tool definition changed` | real edits | ❌ reported |
| provider **MISS** with an intact prefix | cache TTL expiry / eviction | ❌ reported |

Sub-agent requests and llama-server slots: with `--heal`, the main conversation is pinned to `id_slot: 0` and sub-agents rotate through the other slots, so a short sub-agent prompt can't evict the main agent's KV cache.

## Install

```bash
cargo install --path .        # from a checkout
```

Prebuilt binaries and the `npx kvguard` / `uvx kvguard` wrappers are not published yet.

## Usage

### Wrap a command (most common)

```bash
kvguard [--heal] [--port 4141] [--target URL] -- <command> [args...]
```

kvguard starts the proxy on `127.0.0.1:4141` (or a free port), runs the command with `ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL` and `OLLAMA_HOST` pointing at it, stays silent while the tool owns the terminal, and prints a summary when it exits. The wrapped command's exit code is passed through.

Upstreams default to your original `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` / `OLLAMA_HOST`, else the official APIs; `--target` sends everything to one server.

Open the live TUI from a second terminal:

```bash
kvguard attach
```

### Standalone proxy + TUI

```bash
kvguard --heal            # full-screen TUI, then point clients at the printed URL
kvguard --no-tui          # one line per request (logs, servers)
```

```
┌ kvguard v0.1.0 · http://127.0.0.1:4141 ───────────────────────────────────────────────────────────┐
│ 🔥 AUTO-HEAL: ON  │  CACHE HIT 97.4%  │  TOKENS SAVED 1.42M tok  │  $4.82 saved | $0.14 wasted  │ … │
└───────────────────────────────────────────────────────────────────────────────────────────────────┘
 Turn  Branch / Slot    Model               Prefix Health  Hit %  Root Cause / Action
 #04   main [slot 0]    claude-sonnet-4-5   [█░░░░░░░░░]   12%    ❌ BUST @ tok ~#418 (volatile header/id in system · system[0].text)
 #05   main [slot 0]    claude-sonnet-4-5   [██████████]   96%    ⚡ HEALED (sorted 48 tools + stripped cch= billing header)
```

Keys: `j/k` select · `h` toggle auto-heal · `d` JSON-AST / token view · `f` busts only · `e` export trace (.jsonl) · `q` quit.

### CI gate

Record real traffic, then fail builds that break the prefix:

```bash
kvguard --record trace.jsonl -- my-agent run-scenario
kvguard check trace.jsonl --fail-on-bust --min-hit-rate 0.95
```

`check` accepts `.json` files (one request body, or `{"path": "/v1/messages", "request": {...}}`) and `.jsonl` traces, expands globs itself (PowerShell-friendly), prints a Markdown report and exits `1` on failure:

```
| # | source      | branch | verdict           | reused / parent prefix (est. tokens) | root cause                                    |
| 3 | turn-3.json | main   | ❌ BUST @ tok ~#44 | 44 / 105                             | history message mutated at `messages[1].content` |
```

### Options

| Flag | Meaning |
|---|---|
| `--heal` | Enable heal rules (toggle live with `h`) |
| `--target URL` | Single upstream for every API family |
| `--backend auto\|llama-cpp\|ollama\|generic` | Upstream type; `auto` probes llama-server `/props` and `/slots` |
| `--slots N` | llama-server slot count override |
| `--relocate-git-status` | Move Claude Code's `gitStatus:` section from the system prompt to the newest turn (replayed byte-identically on later turns) |
| `--normalize-think-tags` | Strip `<think>` / `reasoning_content` from historical assistant turns (local backends only) |
| `--chat-template FILE` | `.jinja` or `tokenizer_config.json` used to detect template drift |
| `--record FILE` | Append forwarded request bodies to a JSONL file |
| `--prefill-tps N` | Reference prefill speed until llama-server timings calibrate it (default 850) |

## How it works

```
client ──► axum handler ─┬─ heal prefix rules (tools JCS, header strip, timestamp pin, …)
                         ├─ block hashing (xxh64 per tool / system part / message)
                         ├─ lineage tree: continuation | mutation | new branch
                         ├─ routing hints (cache_control, prompt_cache_key, id_slot)
                         └─ forward ──► upstream
upstream ──► UsageTap (relays every chunk immediately, parses usage on the side) ──► client
background ─► token diff (o200k) + JSON-AST diff + chat-template render ─► monitor ─► TUI / attach / summary
```

Inline work is hashing and comparisons only; tokenization and diffing run off the request path, and response bodies are streamed chunk by chunk, so time-to-first-token is unchanged.

## Accuracy and safety notes

- **Token positions are estimates.** kvguard counts with OpenAI's `o200k_base`. That is exact for GPT-4o/GPT-5-family models and approximate for Claude (whose tokenizer is not public) and local models. JSON paths and byte offsets are exact; dollar and hit-rate figures from provider `usage` fields are exact; everything labeled "est." is not.
- **The billing header is only stripped for non-Anthropic backends.** The official API uses it; local servers hash it into the KV prefix.
- **Breakpoints are only added when the client sent none**, so client-managed caching is never overridden and the 4-breakpoint limit can't be exceeded. Adding breakpoints costs cache writes (1.25× on Anthropic).
- **Toggling heal mid-session rebuilds the prefix once** (the request shape changes).
- **Pinned timestamps mean the model sees the session start time**, not the current time.
- **Prices** are list prices per million input tokens, built into the binary; unknown model versions fall back to their family.
- **Clients that don't honor base-URL variables can't be intercepted.** Cursor sends requests through its own backend, so a local proxy never sees them.
- **Ollama does not report reused tokens**, so its hit rate is estimated from the request diff.
- The control endpoints (`/__kvguard/state`, `/__kvguard/heal`) and the proxy listen on loopback only. Recorded traces and exported files contain full prompts; treat them like source code.

## Compared to

| Tool | Approach | kvguard difference |
|---|---|---|
| caching.ai | Docker + Postgres + web console | single binary, local backends, token-level diff |
| CacheLint | static analysis | runtime: sees the prefixes your agent actually sends |
| contextio | raw request logs via mitmproxy | branch-aware diffing and in-flight healing |
| claude-code-cache-fix, ccstatusline | Claude Code–specific | any Anthropic/OpenAI/Ollama-compatible client |

## Development

```bash
cargo test                           # unit tests + 5 end-to-end tests (wiremock upstream)
cargo clippy --all-targets -- -D warnings
vhs demo/kvguard.tape                # regenerate the demo GIF
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
