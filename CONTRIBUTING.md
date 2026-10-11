# Contributing

## Setup

```bash
rustup toolchain install stable
cargo test --workspace
```

`se-core` must stay dependency-free (std only). `se-server`/`se-cli` are
std-only by design too — do not add web frameworks or CLI parsers without
discussing first.

## Rules

- Conventional commits (`feat:`, `fix:`, `perf:`, `test:`, `docs:`,
  `chore:`), one logical change per commit.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`
  and `cargo test --workspace` must all pass (CI enforces this).
- Tests for non-trivial logic: unit tests plus the brute-force oracle in
  `se-core/tests/property.rs` for anything touching scoring or matching.
- Benchmark-affecting changes: commit first, then re-run the full bench and
  report before/after in `docs/BENCHMARKS.md`. Never tune after seeing
  numbers without a full re-run. The frozen harness is `tools/bench.sh` +
  `tools/bench_queries.txt`.
- Numbers in docs must come from real runs on real data; label anything
  synthetic.
