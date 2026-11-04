# kvguard

[![npm](https://img.shields.io/npm/v/kvguard?color=cb3837&logo=npm)](https://www.npmjs.com/package/kvguard)
[![PyPI](https://img.shields.io/pypi/v/kvguard?color=3775a9&logo=pypi&logoColor=white)](https://pypi.org/project/kvguard/)
[![CI](https://github.com/tranhoangtu-it/kvguard/actions/workflows/ci.yml/badge.svg)](https://github.com/tranhoangtu-it/kvguard/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](https://github.com/tranhoangtu-it/kvguard#license)

**Catch 1-token prompt-cache misses in real time — and heal them before the request leaves your machine.**

Prompt caches (Anthropic, OpenAI) and local KV caches (llama.cpp, Ollama, vLLM) only reuse work when a new prompt starts with *exactly* the same bytes as the last one. One rotating header, one timestamp, one reshuffled MCP tool list — and 30–80K tokens are billed or prefilled again. Nothing errors. You just pay more and wait longer.

kvguard is a local proxy + terminal UI that shows you **where** the prefix broke, **why**, and **what it cost** — and can rewrite requests so it doesn't break again.

## Quick start

```bash
npx kvguard -- claude                                          # Claude Code (observe only)
npx kvguard --target http://127.0.0.1:8080 -- opencode  # any agent → llama-server
npx kvguard attach                                             # live TUI in a second terminal
```

The first run downloads the native binary for your platform from [GitHub Releases](https://github.com/tranhoangtu-it/kvguard/releases) and verifies its SHA-256 checksum. No Docker, no database, no account.

## What you see

```
 Turn  Branch / Slot    Model               Prefix Health  Hit %  Root Cause / Action
 #04   main [slot 0]    claude-sonnet-4-5   [█░░░░░░░░░]   12%    ❌ BUST @ tok ~#418 (volatile header/id in system · system[0].text)
 #05   main [slot 0]    claude-sonnet-4-5   [██████████]   96%    ⚡ HEALED (sorted 48 tools + stripped cch= billing header)
 #06   sub-1 [slot 1]   claude-haiku-4-5    [··········]    –     ◆ NEW BRANCH
```

## What it catches

| Cause | Typical source | `--heal` |
|---|---|---|
| tools[] order shuffled | MCP servers starting asynchronously | ✅ sorted by name |
| schema key order changed | non-deterministic JSON serialization | ✅ RFC 8785 key order |
| volatile header in system | Claude Code `cch=` billing header on non-Anthropic backends | ✅ stripped |
| timestamp in prefix | `Current time: …` with seconds | ⚙️ opt-in `--pin-timestamps` |
| chat-template drift | Qwen/DeepSeek dropping `<think>` from old turns | ✅ `--normalize-think-tags` |
| history message mutated | compaction, truncated tool output | 🔎 reported with JSON path |
| sub-agent evicting main KV slot | llama-server with parallel slots | ✅ main pinned to `id_slot 0` |

Sub-agents and parallel sessions get their own branch, so they are never reported as false busts.

## CI gate

```bash
npx kvguard --record trace.jsonl -- my-agent run-scenario
npx kvguard check trace.jsonl --fail-on-bust --min-hit-rate 0.95
```

Prints a Markdown report and exits `1` when a request breaks its parent's prefix.

## Supported

Anthropic `/v1/messages` · OpenAI `/v1/chat/completions` and `/v1/responses` · llama.cpp `llama-server` · Ollama · any OpenAI-compatible server. Prebuilt for Linux (x86_64, arm64), macOS (Intel, Apple Silicon) and Windows (x86_64).

Also available as `uvx kvguard` / `pipx run kvguard` and as a [standalone binary](https://github.com/tranhoangtu-it/kvguard/releases).

**Full documentation, accuracy notes and design:** https://github.com/tranhoangtu-it/kvguard

## License

MIT OR Apache-2.0
