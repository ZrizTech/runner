# contract

This folder holds the wire protocol between the zriz cloud and the runner.

- `*.json`: JSON schemas for frames, ops, results, errors and the exchange.
- `fixtures/frames/`: valid and invalid examples, used by `tests/contract_test.rs`.
- `log/`: the log line format: allowed keys, components, test cases, line regex.

The protocol is versioned. The cloud, this runner and `zz` test against the same files.
They are vendored copies. Change them upstream, not here; a change made only here is lost on the next sync.
