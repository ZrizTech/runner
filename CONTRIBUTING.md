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

## Release checklist (maintainers)

1. Set `version` in `Cargo.toml` on `main`. The tag must equal it (`v1.2.3` for `1.2.3`).
2. Optional dry run: run the `release` workflow by hand (Actions tab). It builds both images and pushes nothing.
3. Tag the commit on `main` and push the tag: `git tag v1.2.3 && git push origin v1.2.3`. The workflow runs CI, then builds, pushes, signs and attests `ghcr.io/zriztech/runner` and `ghcr.io/zriztech/worker`. It fails if the version exists, if the commit is not on `main`, or if CI fails.
4. First release only: GHCR makes a new package private. For each of `runner` and `worker`, open the package page, then Package settings, then Change visibility, then Public. The README says the images are public.
5. Check a pull with no login, and run the two verify commands from the README (`cosign verify`, `gh attestation verify`).
6. Check the tags: `1.2.3` is new. `1.2` and `latest` move only for the highest stable version.
