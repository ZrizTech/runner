# Contributing

Build and check:

    cargo build
    cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
    cd worker && npm ci && npm test

The browser tests in `worker/` need Chromium: `npx playwright install chromium`.

Rules:

- No `unsafe` (the crate forbids it).
- No `unwrap`, `expect` or `panic` outside tests. Clippy denies them.
- Logs never carry payloads: only ids, kinds, reasons and timings. No args, bodies, rows, tokens or URLs with credentials.
- Add a test with every change. A test must fail if a security rule stops holding.
- The wire protocol in `contract/` is vendored. Change it upstream, not here.

Report security problems as described in `SECURITY.md`, not in a public issue.
