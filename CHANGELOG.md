# Changelog

kvguard follows [Semantic Versioning](https://semver.org/). While it is 0.x, minor releases may
change CLI flags, metric names and JSON fields; such changes are listed under **Changed**.

## 0.3.0 — 2026-11-04

### Security
- Control endpoints require a random per-process token (`x-kvguard-token` / `Bearer`);
  `--control-token` / `KVGUARD_TOKEN` sets a fixed one for scrapers. `attach` and `status` read
  it from the attach file.
- Traces, exports and the attach file are owner-only (`0600`) on Unix, in a per-user directory.
- Release binaries carry GitHub build-provenance attestations; workflow actions are pinned by SHA.

### Changed
- `--heal` applies only rules that keep what the model sees: timestamp pinning moved to the
  opt-in `--pin-timestamps`.
- Slot routing never overrides an `id_slot` the client set.
- `--normalize-think-tags` strips reasoning only before the latest user turn (the current tool
  loop keeps its thinking) and also matches `<thinking>` and `[THINK]`.

### Added
- `kvguard check` prints a PASS/FAIL result line, a symbol legend and an estimated list-price
  cost per bust; `--prices FILE` overrides list prices (proxy and `check`).
- Official GitHub Action (`uses: tranhoangtu-it/kvguard@v0.3.0`).
- Homebrew: `brew install tranhoangtu-it/tap/kvguard`; the release workflow renders the
  formula from the release checksums, test-installs it on macOS and pushes it to the tap.
- `kvguard_recent_turns{model,status}` gauge on `/__kvguard/metrics`.
- Ollama detected behind its OpenAI-compatible `/v1` endpoint.
- Reproducible latency benchmark (`examples/proxy_overhead.rs`, `docs/benchmarks.md`),
  `SECURITY.md`.

## 0.2.0 — 2026-11-04

### Added
- Cache audit: `PARTIAL` (intact prefix, provider served less than expected) and provider-truth
  waste (`expected − served`) with TTL-aware write prices.
- Conservative per-model minimum cacheable sizes; Anthropic 20-block lookback and OpenAI
  per-key rate notes.
- Exact llama-server token positions via `/apply-template` + `/tokenize`, with automatic
  chat-template drift detection; single-slot advice.
- Prometheus endpoint, `kvguard status --line`, `--partial-threshold`, opt-in trusted publishing.

## 0.1.1 — 2026-11-04

### Fixed
- npm and PyPI launchers ship with a README.

## 0.1.0 — 2026-11-04

- First release: streaming proxy, lineage tree, JSON-AST + token diff, heal rules, TUI,
  `attach`, `check`, npm/PyPI launchers, binaries for five targets.
