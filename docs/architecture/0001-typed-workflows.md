# ADR 1: Typed workflows and replaceable browser transports

Status: accepted design; implemented in part. The [execution ledger](EXECUTION.md) records what is verified and what remains.

## Context

The existing Rust model mixes transport JSON, string-valued operators and modes, independent status fields, and mutable browser/session state. Browser methods combine transport with secret resolution, settling, and implicit tab replacement. Scripts have dynamic values by design; their successful semantics must survive the refactor.

## Decision

Decode untrusted DTOs into validated types. Keep executable programs opaque, correlate typed completions to their pending task/effect/attempt/revision, and keep capabilities nonserializable. Use composable workflow states with an asynchronous runner, not a generic untyped event interpreter.

Separate a shared browser runtime from object-safe driver traits with boxed Send futures. Page ownership is exclusive, capabilities are usable interfaces, and drivers report dispatch evidence rather than business success. Native input strategies may differ by capability; workflows do not branch on backend names.

Persist task continuations and external effects transactionally in SQLite, with FULL durability, explicit ownership locks, and revision checks. A dispatched effect without a confirmed receipt requires reconciliation. Resume is explicit and local to the machine. An unsupported or inconclusive recovery pauses.

## Consequences

Validated construction and phase-specific payloads prevent internal contradictory states. Runtime checks are still required for changing pages, foreign completions, and uncertain effects. Persistence introduces disk latency and schema compatibility obligations. Driver and workflow tests can run against deterministic fakes; real-browser conformance is still required.

Successful script semantics, decision thresholds, legacy response payloads, and benchmark outcomes remain compatibility contracts. Do not replace them with conventional language semantics accidentally. Never fall back to the old execution path after the new path has dispatched mutations.

## References

- [Rust dyn compatibility](https://doc.rust-lang.org/reference/items/traits.html#dyn-compatibility)
- [SQLite WAL durability and version requirements](https://www.sqlite.org/wal.html)
- [Documenting architecture decisions](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)
- [Intent as a current, versioned document](https://github.com/roboco-io/intent-engineering)
