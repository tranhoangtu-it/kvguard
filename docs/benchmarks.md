# Benchmarks & validation

What has been measured, how to reproduce it, and what has **not** been measured yet.

## Proxy overhead

kvguard parses and hashes every LLM request on the request path, then streams the response
through untouched. To measure the cost, `examples/proxy_overhead.rs` runs a local mock upstream
that answers immediately with a 40-chunk SSE stream, and interleaves direct and proxied requests.
Each request carries an Anthropic-style conversation of **~315 KB (~80K tokens)** with 30 tools,
growing every turn.

```bash
cargo run --release --example proxy_overhead          # 300 turns per mode
cargo run --release --example proxy_overhead -- 1000  # more samples
```

Result on Windows 11, x86_64, release build, 300 turns per mode (2026-11-04):

| Time to first byte | p50 | p95 | p99 |
|---|---|---|---|
| direct | 0.29 ms | 0.66 ms | 17.53 ms |
| via kvguard (inspect) | 1.29 ms | 3.08 ms | 15.84 ms |
| via kvguard (`--heal`) | 1.70 ms | 2.83 ms | 15.91 ms |

| Total response time | p50 | p95 | p99 |
|---|---|---|---|
| direct | 0.70 ms | 3.06 ms | 18.04 ms |
| via kvguard (inspect) | 1.93 ms | 4.48 ms | 16.40 ms |
| via kvguard (`--heal`) | 2.35 ms | 5.34 ms | 16.59 ms |

**Reading:** kvguard adds about **1.0 ms** (inspect) and **1.4 ms** (heal) at the median for an
80K-token request — JSON parsing, per-block hashing, lineage matching and (with heal)
canonicalization. Real model APIs take hundreds of milliseconds to seconds to their first token,
so the added latency is well under 1 %. p99 is dominated by OS scheduling on the test machine
and is similar with and without the proxy. Token counting and diffing run off the request path
and are not part of these numbers.

## Real traffic

| Client → backend | What happened | kvguard verdict |
|---|---|---|
| Claude Code → Anthropic API (`claude-opus-5-5`) | turn 1 wrote 95,836 tokens to cache; turn 2 (`--continue`) read exactly 95,836 | `NEW BRANCH`, then `HIT` with expected = served = 95,836 |
| OpenAI-compatible client → Ollama 0.35 (`qwen2.5:7b-instruct`) | turn 2 served 1,619 of 1,635 prompt tokens from cache | `HIT` (98 %); timestamp pinned with `--pin-timestamps` |

## Not measured yet

Be skeptical of anything not in the tables above. In particular, kvguard has **not** yet published:

- a false-positive / false-negative rate for lineage matching on a large corpus of real sessions;
- before/after API invoices for a team using `--heal`;
- prefill-time savings on llama-server under a real agent workload (A/B with and without heal).

Contributions of anonymized traces (`kvguard --record trace.jsonl -- …`, with prompts redacted)
are the fastest way to fill these gaps.
