# Refactor execution ledger

Status: the active development phase is implemented and verified as recorded below. The accepted architecture is not complete: see [Remaining gaps](#remaining-gaps). Entries describe verified work, not completion promises.

## Baseline

The pre-refactor checkout contains substantial user changes, including untracked scripting, scraping, and pool code. Preserve them. A tracked binary patch, untracked archive, status listing, and test output were captured at `/tmp/usebrowser-refactor-baseline` before implementation. Baseline: 90 workspace tests passed, 1 ignored.

## Verified implementation

Intention IDs refer to [INTENT.md](../../INTENT.md).

### Typed domain (I01, I06) — `fab-core/src/domain.rs`

- Validated `TaskId` and `Probability`; process-local `PageId` and `ObservationStamp`.
- Observation-bound text/select/check/radio/click targets. `TextInput` privately classifies literal vs. placeholder text.
- `ActionBatch` validates nonempty work, mutually exclusive click/Enter, target kinds, and one observation stamp. `agent.rs` converts decision plans into it before execution.
- `Session` has a stable `PageId`; `live_key` rejects targets from replaced documents. `Session::from_browser` accepts injected drivers.
- `jev::Answer` carries `Probability` for noul, probability maps, and confidence (compatibility accessors return `f64`).

### Scripts (I02, I03) — `fab-core/src/script.rs`, `fab-core/src/program_machine.rs`

- Typed `BinOp`/`Builtin`; opaque, validated `Program`; typed `EvalError`; dynamic values keep float bits and existing coercions.
- `ProgramMachine` is a serializable explicit machine: `advance()` yields `Request`, `Record`, or `Finished`; completions carry correlated tokens and stale/foreign tokens are rejected.
- Completions must have the shape the request promises: `test`/`next page` answer `Bool`, `items` answers `List`, navigation answers `Done`, exhausted iterators answer a batch. A mis-shaped completion is rejected and leaves the request pending (`RequestKind::accepts`).
- Checkpoint deserialization validates program counter, iterators, pending request/instruction agreement, and the shape of replayed expression answers.
- `api.rs` `run_script` drives the machine; the old `Fx`/`eval_fx` helpers are gone. Short-circuit and nested eager-effect ordering are covered by tests.

### Browser drivers (I04, I05, I06) — `fab-core/src/backend/`

- `Browser` owns `Mutex<Box<dyn PageDriver>>`. Optional capabilities are borrowed interfaces: `Pointer::{Coordinates, Targeted}`, `Text::{Focused, Targeted}`, `EnterKey`, `Renewable`. CDP, BiDi, and Camofox implement them; absent capabilities cannot dispatch.
- `InputError::{NotSent, MayHaveExecuted}`; `InputReceipt` is transport acknowledgment only. `eval`/`health` no longer replace lost pages silently; renewal is explicit.
- Hosts are `Option<Arc<dyn BrowserHost>>` (`page()`, `shutdown()`), with CDP/BiDi adapters.
- Uncertainty is no longer flattened into a retryable string: `ActResult.uncertain` is set when an action error chain holds `MayHaveExecuted` (`agent::may_have_executed`), `do_goal` reports status `uncertain`, `tools::call` returns `Status::{Ok, Failed, Uncertain}`, and the planner ends the run instead of letting the model repeat the action.

### Durable journal (I07, I08, I10) — `fab/src/task_store/`

- SQLite WAL, `synchronous=FULL`, minimum SQLite 3.51.3; exclusive task and session OS locks (released on process death, covered by a subprocess test).
- Revision CAS; effects go Prepared → Dispatched → Confirmed/Uncertain with typed `EffectKind::{ToolCall, ProgramRequest, ProgramNavigation}`; receipt, checkpoint, and outbox commit atomically.
- Deduplication by session and request id in an immediate transaction with a unique constraint; concurrent duplicate submissions from separate connections admit one task.
- Cancellation is terminal and preserves uncertain effects. Recovery treats Prepared effects as not dispatched without executing them.
- A reconciled task becomes Paused, never Queued: only an explicit resume continues it, so a duplicate submission after resolution cannot restart work.

### Application integration (I02, I07, I08, I10) — `fab/src/task_runtime.rs`, `durable_program.rs`, `task_commands.rs`

- `do`/`step` journal one whole-call effect; a failed or interrupted call pauses with uncertainty. `run` journals each program request.
- When a `do`/`step` ends in a planner loop (agent mode's planner path or fallback), each of the loop's tool calls is its own `EffectKind::PlannerCall` (`{tool, args}`, receipt `{tool, text, status}`, redacted), journaled through `planner::CallJournal` (`task_runtime::PlannerJournal`). Boundary: the loop is the tail of the call, so the work before it (navigation, sign-in, engine attempt, compiled steps) has returned when the first planner call closes the whole-call effect (checkpoint `{planner_calls: 0}`); each confirmed call checkpoints `{planner_calls: n}`. A durable run stops before the planner when the engine result is unconfirmed; `run_script` clears the journal, so planners inside a program's `do` steps stay under the whole call. A crash leaves at most one planner call uncertain. Such a task cannot resume yet (it would repeat confirmed calls): resolve it `applied` with the whole call's reply, or cancel it.
- Explicit resume, completed-receipt recovery, cooperative cancellation through an active-runner registry, and session-close interrupts.
- Program checkpoints hold the machine, scraper, a bounded progress-log tail, counters, and a count of committed records (records live only in the outbox, so checkpoints no longer grow with output). Records commit before live streaming; resumed runs do not re-emit them.
- Checkpoints containing registered concealed material are rejected. Item handles held by the machine must exist in the saved scraper.
- The scraper deserializes through a validated DTO (open item, virtual page, fetched URLs, field maps, seen/items agreement, accounting).
- Resume checks page, document, and revision before dispatch; `--adopt-page` accepts a replacement page. With a detail page open it first returns to the recorded list page as its own journaled effect (`EffectKind::ProgramReturn`: a `goto` of the recorded address, or nothing for a fetched page), continuing from a checkpointed reopen point (the program suspended at the outermost `open`), so the item is opened again. Refused (paused) when that could repeat work: a record emitted, or a `do`/agent-mode `read` dispatched, since the open. An interrupted return resolves `not_applied` (return again) or `program_applied true` (arrival checked like a navigation).
- Typed commands: list/show/output/resume/cancel/resolve. Resolution: `applied` (whole-call reply), `program_applied` (the value of a `do`/`read` leaf, a boolean for `test`, or `true` for an initial navigation or a return), `not_applied`, and `observed` (`fab tasks resolve --observe`, run in the session on the request's own page) for `items`/`next page` and for the two navigations: the list's keys on the live page are reconciled with the saved `seen` keys. `items` is answered with the unseen items (read in code with the saved mapping); `next page` resolves applied (`true`) when unseen items show, `not_applied` when only read ones do; `open`/`back` resolve applied when the address on screen is the one they were going to, and `not_applied` otherwise (which is never proof that it did not happen, and repeating a navigation is safe). Refused (`invalid_args`) for another page or document, an empty list, a list/fields never mapped, an `open` whose page was fetched, or a `back` with no list page. Scalar evidence for scraper requests stays rejected.
- A journaled `do`/`step` pauses only when it may have changed something: an action the engine committed, a planner call it made, or a call stopped midway. A failure decided between actions is final whatever ran before it, and a call that committed nothing and never reached its planner finishes as the failure it is (`leaves_uncertain`).

### Secrets (I09) — `fab-core/src/secret_machine.rs`, `fab-core/src/secrets/`

- Generation → save → reconcile → type phases over opaque references; operation ids carry a random workflow epoch, so a completion from another workflow is rejected. Checkpoints hold no secret bytes (scanned per phase).
- `generated_for` and every resolution path (`resolve`, `substitute`, `substitute_for_input`) return a generated password only after a confirmed save.
- A save that was never dispatched (`SaveNotSent`: no store, helper did not start, helper exit 1) retries the same value and may fall through to the next store; a dispatched failure stops and requires reconciliation, never regeneration. Store errors during reconciliation count as unconfirmed.
- Workflow references survive a daemon restart (`secrets/workflows.rs`): `<state dir>/secret-workflows.json` (0600, locked read-modify-write, temp file + rename) holds per site the origin, username and `Checkpoint`, never the password. It is recorded before a save is sent and after each completion; a never-sent save is forgotten. The vault restores it at start and re-reads the site's record on each use. A restored save reconciles by looking up the site's login for that username in the store (listing and full-item caches dropped first); if found, the store's material is typed, otherwise the site stays paused, across restarts too. `fab secrets reset --url` clears a paused or saved workflow (refused while a save still needs reconciling); a running daemon picks it up on its next use of the site.
- A second save for a site while one is in flight fails with "already in progress".

### Agent surface and output contract (2026-09-26, pushed)

- The MCP server is removed; agents use the CLI and `fab --skill` (bench-only chrome-devtools-mcp client kept). Unused deps dropped.
- One output contract (`crates/fab/src/events.rs`, [docs/output.md](../output.md)): stdout is JSONL `start`/`record`/`item`/`end`; `end` on success and failure with a typed `error.code`; exit codes 0/1/2/3 follow it. Records commit to the task outbox before printing (whole `do` calls included), so duplicates and `fab tasks output` replay them. Conformance test: `crates/fab/tests/output_contract.rs`.
- Schema-first requests (`crates/fab/src/request.rs`): `fab do '{"do", "url", "records", "returns"}'` with flat-scalar JSON Schema; the compiler is constrained to the declared fields; records are conformed (order, coercion) or the run ends with `schema_mismatch` (a definite failure, not a pause). `fab --schema` prints request and event schemas.
- Live: Chrome, structured request on the jobs fixture reproduced gold exactly (55/55); kill-daemon stream ends `interrupted` with the delivered record counted and `tasks output` replays it.

## Verification (2026-09-26)

- Workspace tests: 95 fab-core + 53 fab + 1 compile-fail doctest pass; 1 live test ignored by default. Scraper round-trip test normalizes set order (it was order-dependent).
- Ignored live pool test (separate tabs, hangs, `chrome://crash`, facade close): passes on Chrome.
- Release build and tests (`cargo test --release --workspace --locked --offline`): same counts, all pass.
- CLI on real browsers, isolated headless profiles, local fixtures, sessions closed afterwards:
  - Chrome: invalid program rejected before a session starts; pure program emits two records; duplicate `--request-id` returns the same task and reply without re-streaming; output cursors replay correctly.
  - Chrome and Firefox: native text input into `fixture:signup.html` read back the typed value.
  - Chrome crash recovery: daemon killed with SIGKILL mid-run → task interrupted with the in-flight request uncertain → resume refused until resolved → duplicate submissions refused before and after resolution → `program_applied` leaves the task paused → plain resume in a new browser pauses on the page binding → `resume --adopt-page` completes; the outbox holds each record once.
- Not verified: Firefox crash recovery, Camofox, secret workflows against real password managers, benchmark suites.

## Remaining gaps

- `do`/`step` use conservative whole-call journaling. Goal, action, scrape, and secret steps are not nested durable machines, so an interrupted call is reconciled as a whole. Planner loops journal each call but keep no messages: resuming from a planner checkpoint (stage 2) needs the LLM transcript persisted and a page check before the next call.
- After a restart, reconciliation cannot compare against the generated value (it is not persisted): any login in the store for the site and username counts as the saved one.
- Scraper requests other than `items`/`next page` (`open`, `back`, `extract`, implicit pagination) have no evidence resolution: only `not_applied`. Checkpoints written before reopen points existed cannot return from an open detail page (adoption pauses). The `pager` flag a `next page` would have set is not recovered by observation.
- An applied navigation resolution records the asserted address; the resumed run checks origin and path before binding the page and pauses otherwise (not a full re-observation of content).
- Checkpoints still grow with items scraped: forgotten items keep their list and key, and `seen` keeps every key (fields, links, and extra fetched pages are dropped; see bench/LOG.md).
- The script parser is hand-written; a declared grammar with a generated parser is planned (and removal of regex from the in-page runtimes, including the mapper's per-field `re`).
