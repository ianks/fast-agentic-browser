# Contributing

## Build and test

```bash
cargo build --release --features bench   # fab, and fab-bench (the evals)
cargo test --workspace --locked --all-features
```

## The scrape eval

`fab-bench scrape gen` generates the fixture sites and their gold records, and
`fab-bench scrape run` runs the cases through the `fab` CLI and scores them.
The run calls an LLM, so it needs `OPENROUTER_API_KEY` (in the environment or
in `~/.config/fab/env`):

```bash
cargo run --release --features bench --bin fab-bench -- scrape gen
cargo run --release --features bench --bin fab-bench -- scrape run
```

## Measured changes

`bench/LOG.md` records every change that was measured, with its result. If
your change affects speed, accuracy or cost, run the relevant suite and add an
entry.

## Publishing

Bump the version of each changed crate, commit, push, then `just publish`. It
runs the tests and publishes every crate whose version is not on crates.io
yet, `fab-core` first.
