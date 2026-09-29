# fab: coding intentions

This document records accepted intent, not a claim that every intention is already implemented. The implementation and its tests must be checked against it. Change an intention deliberately, with its rationale; do not weaken it to accommodate an implementation failure.

## Purpose

fab lets people and agents complete browser tasks quickly, accurately, and with little model overhead. Users express goals and program logic; the engine handles live-page interactions. Keep the successful CLI and script experience stable while making the internals understandable and difficult to misuse.

## Accepted intentions

| ID | Intention | Evidence required |
| --- | --- | --- |
| I01 | Make invalid internal combinations impossible through supported APIs. Use closed enums, private constructors, and checked boundary conversions. | Compile-fail examples and invariant tests. |
| I02 | Model execution as explicit, composable states with an asynchronous I/O runner. Each phase carries only applicable data. | Transition tests, including wrong-phase and stale completions. |
| I03 | Preserve successful script coercions, argument permissiveness, field ordering, and effect evaluation order. Reject statically detectable structural errors before browser effects. | Characterization and parser/evaluator tests. |
| I04 | Browser drivers implement transport, not policy. Shared code handles observation, resolution, sequencing, settling, and recovery. | The same conformance tests across concrete and fake drivers. |
| I05 | Capabilities are callable interfaces, not independent support flags. Never silently downgrade trusted or secret input. | Missing-capability and input-path tests. |
| I06 | Page identity is distinct from URL. Only one owner mutates a page; loss, replacement, popup adoption, and reconnection are explicit. | Ownership and stale-target tests. |
| I07 | Persist intent before external dispatch and persist receipts with continuation advancement. A lost response is uncertainty, not proof of failure. | Crash injection at every journal/dispatch boundary. |
| I08 | Recovery is same-machine and explicitly requested. Reconcile uncertain effects or pause. Never restart cancelled work or blindly replay mutations. | Restart, cancellation, duplicate-resume, and reconciliation tests. |
| I09 | Persist references to vault secrets, never resolved secret values. Generated passwords are saved before website use and not regenerated on ambiguous recovery. | Secret-leak scans and interrupted-save tests. |
| I10 | Store output records and continuation advancement atomically. Preserve JSONL payloads and expose explicit cursor replay. | Outbox uniqueness and interrupted-delivery tests. |
| I11 | Preserve configured decision policies, batching, and speculative model concurrency. Keep benchmark accuracy and latency visible. | Existing browser suites and paired benchmarks. |

## Limits

- Rust types cannot prove a live web page will remain unchanged; runtime checks and typed uncertainty remain necessary.
- Neither browser mutations nor terminal output have exactly-once delivery guarantees.
- This refactor does not introduce cross-machine recovery, dynamic library loading, or a statically typed user scripting language.
- Durable data does not deserialize into live leases, resolved targets, or execution permissions.
- In this phase, `do` and `step` are journaled as one whole-call effect; their inner goal, action, scrape, and secret steps are not individually durable.

## Maintenance

The coordinator owns this document and shared contracts during parallel work. Workers cite intention IDs and link verification evidence. Keep this file current; Git retains history. Architectural rationale belongs in [decision records](docs/architecture/0001-typed-workflows.md), and current implementation status belongs in [the execution ledger](docs/architecture/EXECUTION.md).
