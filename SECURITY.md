# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through
[GitHub security advisories](https://github.com/tranhoangtu-it/kvguard/security/advisories/new).
Do not open a public issue. You can expect an acknowledgement within 7 days.

## Supported versions

Only the latest minor release receives security fixes while kvguard is 0.x.

## Security model

kvguard is a local reverse proxy: every LLM request, including API keys in headers and full
prompts in bodies, passes through it.

- **Network:** listens on `127.0.0.1` only. Requests are forwarded only to the upstream you
  configured (`--target`, or your original `*_BASE_URL` variables, else the official API).
  Redirects are never followed. There is **no telemetry**.
- **Credentials:** request headers (API keys, OAuth tokens) are forwarded and never logged,
  recorded or exported.
- **Control endpoints** (`/__kvguard/state`, `/heal`, `/metrics`) require a random per-process
  token (`x-kvguard-token` or `Authorization: Bearer`), a loopback `Host` and no browser `Origin`
  — blocking other local users, DNS rebinding and CSRF.
- **Files:** `--record` traces, TUI exports and the attach file contain prompts or the control
  token. On Unix they are created `0600` in an owner-only directory (`$XDG_RUNTIME_DIR` when set);
  on Windows they live in the per-user `%TEMP%`. Treat traces like source code.
- **Request rewriting** happens only with `--heal` (and the opt-in rewrite flags). Inspect mode
  forwards request bodies byte-for-byte.

## Verifying release binaries

Each release asset ships with a SHA-256 checksum and a
[build provenance attestation](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations)
signed by GitHub Actions:

```bash
gh attestation verify kvguard-v0.3.0-x86_64-unknown-linux-musl.tar.gz --repo tranhoangtu-it/kvguard
```

The npm and PyPI launchers verify the checksum before running the downloaded binary; the npm
package itself is published with provenance. For the strongest guarantee, build from source:
`cargo install --git https://github.com/tranhoangtu-it/kvguard --locked`.
