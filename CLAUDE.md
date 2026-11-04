# CLAUDE.md

kvguard is a single Rust binary: a local HTTP/SSE proxy plus a TUI. It catches prompt-cache and
KV-cache prefix breaks, diagnoses them and optionally heals requests for Anthropic, OpenAI,
llama.cpp and Ollama. It is observe-first: requests are byte-identical unless `--heal` is set.

## Commands

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                                   # unit + tests/integration_tests.rs (wiremock)
cargo run -- check tests/fixtures/history-mutation/turn-1.json tests/fixtures/history-mutation/turn-2.json
cargo run --release --example proxy_overhead          # latency benchmark, see docs/benchmarks.md
```

CI (`.github/workflows/ci.yml`) runs lint, tests on Linux/macOS/Windows, MSRV 1.88 and a smoke
test of the composite `action.yml`. Merge only after CI passes on the PR head SHA.

## Layout

| Path | Role |
|------|------|
| `src/proxy/` | axum server and upstream forwarding (reqwest, never follows redirects). Backend detection is in `mod.rs`, and the control endpoints live under `/__kvguard/{state,heal,metrics}`. `local_llm.rs` handles llama.cpp `/apply-template` + `/tokenize` and Ollama |
| `src/lineage/` | Block hashing (tools → system → messages, `cache_control` stripped) and an LCP tree that classifies each turn as Continuation, Mutation or NewBranch |
| `src/diff/` | JSON-AST and token diff, Jinja chat-template verifier, exact llama token positions |
| `src/canonicalizer/` | Heal rules. Safe ones (on with `--heal`): JCS tool sort, cache breakpoints, slot routing. Opt-in ones: timestamp pinning, git-status relocation, think-tag stripping |
| `src/monitor.rs` | Turn records and the cache audit (HIT/BUST/PARTIAL/MISS, waste from provider usage), plus Prometheus output |
| `src/pricing/` | List prices, `--prices` overrides, minimum cacheable sizes |
| `src/ci/` | `kvguard check` and its Markdown report |
| `src/tui/` | ratatui UI, local or attached to a remote proxy |
| `src/wrapper.rs`, `src/secure_file.rs` | Attach file, per-user runtime dir, files created 0600, control token |
| `wrappers/npm`, `wrappers/pypi` | Thin launchers that download the release binary |

## Invariants

- Without `--heal`, forwarded bytes must equal what the client sent. Safe heal rules must not
  change what the model sees. Anything that does change it is an opt-in flag.
- Never override a client-set `id_slot` or existing `cache_control` breakpoints.
- Control endpoints require the per-process token, a loopback Host header and no Origin header.
- Files kvguard writes (traces, exports, attach file) are owner-only.

## Releasing

1. Bump the version in `Cargo.toml` (then `cargo update -p kvguard`), `wrappers/npm/package.json`,
   `wrappers/pypi/pyproject.toml` and `wrappers/pypi/kvguard/__init__.py`. Also bump the Action tag
   in the README and add a CHANGELOG entry.
2. Merge, then tag the bump commit by SHA (`git fetch origin; git tag -a vX.Y.Z <sha> -m ...`);
   tagging after a stale `git pull` once put a tag on the pre-bump commit. Before tagging, a
   build-only trial is `gh workflow run release.yml --ref <branch>` (publish steps skip without a
   tag). `release.yml` builds 5 targets (both macOS targets in one job, because separate macOS
   arm64 jobs were repeatedly cancelled for runner capacity) with provenance attestations
   and creates the GitHub Release. If the repo variables `PUBLISH_NPM`, `PUBLISH_PYPI` and
   `PUBLISH_HOMEBREW` are set, it also publishes to npm and PyPI (OIDC trusted publishing) and to
   the `tranhoangtu-it/homebrew-tap` formula (secret `HOMEBREW_TAP_TOKEN`).
3. Tag pushes and registry approvals (npm staged publishing) are done by the maintainer.

## Conventions

- Rust 2024, MSRV 1.88. Clippy with `-D warnings`; prefer let-chains to nested `if let`.
- Workflow actions are pinned by commit SHA.
- Conventional commits; no plan/phase IDs in code, tests or commit messages.
