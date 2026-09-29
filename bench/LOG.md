# Iteration log

Metric: scripted suite (`ub bench`), pass rate first, then wall time excluding
the initial page load. Knob changes are kept only if pass rate holds and time drops.

## Measured facts

- **Jev via OpenRouter** (`/api/v1/systemone`, `jev-latest` → `typesafe/jev-1.13`):
  cold 600–700 ms (TLS alone ~285 ms), warm HTTP/2 **~220–330 ms**, occasional 500+ ms.
  → keep one warm HTTP/2 connection; warm it while Chrome launches.
- Jev latency is **~flat in payload size** (800–1350 input tokens, 2–6 questions all
  within noise). Trimming state doesn't buy speed; the number of calls does.
- Removing element lines from state (descriptions only in Choice criteria) or nulling
  criteria (descriptions only in state) **lowers click confidence** (0.93→0.49 on a
  date-picker step, 0.61→0.22 on a form). Keep both: state lines + criteria descriptions.
- Cheap planner LLMs (OpenRouter, one small tool call): gemini-3.1-flash-lite ~0.8–1.3 s,
  mercury-2.5 ~1.0–1.3 s, gpt-6-luna ~1.5 s, gpt-5-nano ~1.25 s. A planner turn costs
  4–6 Jev calls, so round-trips to the planner dominate end-to-end time.
- Snapshot (JS walk + JSON) is 1–30 ms even on the 1000-contact page; exec 1–20 ms.

## Timeline

| version | change | pass | Σ p50 (ms) | notes |
|---|---|---|---|---|
| v0 | first working pipeline | 22/24 | 25,386 | combobox, datepicker fail |
| v1 | `auto_filled` list in state; click question says everything else must be clicked; combobox shows displayed value | 23/24 | 26,252 | fixes both; act now fails honestly when done is low (exposed confirm_delete) |
| v1.1 | context no longer strips the element name from section labels ("Delete file?" → "file?") | — | — | fixed confirm_delete done=0.04 |
| v2 | speculative `final` question skips the verification call when p(one more click) ≥ 0.9 | 24/24 | 21,248 | **−19%** |
| v3 | main-world instrumentation bridge (script element + CustomEvents) | 24/24 (camofox) | 21,295 | camofox was 20/24: Camoufox evaluates in an isolated world, so fetch/timer patches were invisible to the page |
| v3 r3 | same, CDP, 3 repeats | 72/72 | — | baseline for knob sweeps |
| v4 | hedged Jev requests (2 identical, first wins) | 72/72 | −11% vs v3 | trims Jev's 500 ms+ tail |
| v5 | hedge=2 + trust_final 0.6 + quiet 20 ms | 72/72 | −15% vs v3 | login 995 → 297 ms (one Jev call) |

## Decisions and why

- **Choice beats Noul for "is this the last click?"** Probed 4 phrasings on 11 labeled
  states: Noul variants 55–82% accurate; Choice `one` vs `several` 91% with zero
  false-finals. The original Noul ("can it be finished with ≤1 click") returned 0.0
  everywhere.
- **Literal wording matters**: "fields are filled automatically" made Jev skip
  comboboxes and date pickers. Naming the auto-filled ids explicitly fixed it
  (Jev reads literally, per its jaggedness notes).
- **Values come from instruction spans.** Quoted strings are used when present, otherwise
  1–4-grams that don't start or end on a stopword. Both quoted and unquoted login pass.

## The real benchmark: agent task completion (control vs experiment)

Scripted mode only measures the tool itself. The benchmark that matters is an LLM
agent completing tasks from a natural-language goal:

- **control**: LLM + chrome-devtools-mcp 1.10.1 (all 30 tools, headless, isolated)
- **experiment**: LLM + usebrowser (act / run / read / extract / observe)

Both arms use the same model (`google/gemini-3.1-flash-lite`, the fastest cheap
tool-caller measured), the same 30-turn budget, the same initial page observation
(taken before the timer), and the same record/answer checks. The LLM-as-decider
baseline was removed.

### What the first head-to-head taught us (base suite, 1 run)

| arm | pass | Σ wall |
|---|---|---|
| control | 22/24 | 320 s |
| experiment v1 | 19/24 | 265 s |
| experiment v2 (fixes below) | **24/24** | **100 s** |

Failures in experiment v1 were tool-design mismatches with how LLMs actually talk:
1. **LLMs point at ids** from the page listing (`type "x" into e1`, `click e6`).
   Jev handled those poorly. → Added a **deterministic fast path** (`direct.rs`):
   precise commands (`click e12`, `type "…" into e5`, `select "M" from "Size"`,
   `check e9`, `press enter`, comma-joined) run with **zero model calls**. Stale ids
   are remapped by description; anything fuzzy falls back to Jev, with ids rewritten
   into descriptions first.
2. **The prompt's `act("…")` example got copied** into `run` lists. → Removed the
   code-like example, and `sanitize()` strips `act(…)` / `run(…)` wrappers.
3. **Pagination results were invisible**: the act result's page summary was capped
   from the top of the page. → Every act/run result now leads with
   **"new on page"**, i.e. the text that appeared since the action started (rows,
   toasts, errors).
4. `read` added: page text with tables as `a | b | c` rows, and usable as a `run` step.

Two-tier API result: most agent steps are now precise id commands (a few ms of tool
time), and Jev handles high-level or fuzzy instructions. Agent time is now dominated by
LLM turns (about 1 s each), so the remaining lever is **fewer turns**: `run` batching
and self-describing results.

### Hard suite (14 realistic tasks, 1 run)

| arm | pass | Σ wall | cost |
|---|---|---|---|
| control | 10/14 | 271 s | $0.129 |
| experiment | 12/14 | 98 s | $0.046 |

Shared failures are planner-LLM reasoning, not tooling. On `orders_refund_total`
both arms saw every refunded row and summed wrong ($272.65 vs $585.10), and on
`shipments_delayed_rotterdam` both miscounted rows that were shown correctly.
Single runs vary a lot with a cheap planner, so the final numbers use 3 repeats.

### Final head-to-head (3 repeats, gemini-3.1-flash-lite planner, 2026-09-23)

| suite | arm | pass | Σ p50 wall | cost |
|---|---|---|---|---|
| component (24) | control (chrome-devtools-mcp) | 69/72 | 325.6 s | $0.364 |
| component (24) | experiment (usebrowser) | 63/72 | 83.7 s (**−74%**) | $0.134 |
| hard (14) | control | 38/42 | 229.0 s | $0.510 |
| hard (14) | experiment | 35/42 | 95.2 s (**−58%**) | $0.147 |

Experiment is 2.4–3.9× faster and 2.7–3.5× cheaper, with **lower accuracy** (−6 and −3 passes).
Failure breakdown:
- **Tool bug (fixed after the run):** `login` 0/3. The LLM wrote a paren-less
  `run "…", "…"` inside `act`, and the sanitizer only handled `run(…)`. After the fix,
  login/login_noquote/checkout go 9/9.
- **Planner behavior, experiment only:**
  - `search_open*` 0/6. The model searched for the product name instead of "wireless
    mouse"; it reached the right page, but the check requires the literal search.
  - `parcelwise_hold_overdue` 0/3. The model found the right account, then called
    `finish` ("I will now proceed…") without acting.
  - `audit_log_revoked_key` 1/3. Hallucinated name (not investigated yet).
- **Control only:** `biglist` 0/3. The 1000-row a11y snapshot overflows the planner's
  context (HTTP 400) or burns the turn budget. usebrowser prunes to relevant rows.
- **Both arms:** `orders_refund_total` 1/3 each. Cross-page arithmetic by the cheap planner.

Next levers: make act/run results say "goal likely incomplete" when the planner
finishes early (Jev `done` Noul on the goal), investigate the audit-log `read` output,
and rerun the full comparison.

## Decision VM, phase notes (2026-09-23)

### P0: honest measurement
- **Strict checks.** Fixtures now record every mutation. Scenarios list `expect` and `allow` events, and any other event fails the run (`check_records(strict)`). `ub rescore` re-applies the checks to stored runs, since each run now keeps its `records`.
  - Rescoring the pilot flipped **4 experiment PASS→FAIL, 0 control**:
    - a wishlist click;
    - the email saved into the profile's name field;
    - an empty review submitted;
    - a junk "Persistence test" expense draft.
- **Harness fixes:**
  - A text-only LLM turn now gets a nudge ("call a tool or finish", at most 2) instead of ending the run. This is what ended parcelwise early.
  - Duplicate (fixture, goal) scenarios are skipped in agent modes.
  - Runs record their `model` and `para`.
  - `ub compare` reports a paired McNemar test.
- **Prompt contamination removed.** The examples in the tool description were `alice`/`hunter2` (a benchmark goal) and `ada@x.io`, which deepseek copied. They are now `<text>`/`<id>` placeholders.
- **Decision dataset.** `ub bench --record` captures each Jev decision from strictly passing runs. `ub decisions paraphrase` adds LLM rewrites whose quoted literals survive verbatim. Current size: 52 scripted + 206 paraphrase cases. `ub decisions eval` replays them with no browser: 258 cases in about 13 s for $0.017.
- **Planner pilot** (12 tasks, canonical goals, strict, old binary). Experiment-arm pass rates:
  - mercury-2.5: 100% at 5.6 s, $0.0014/task;
  - step-3.7-flash: 100% at 10.6 s;
  - gemini-3.8-flash, deepseek-v4.1-flash, glm-5.3-flash: 92%;
  - gpt-6-luna: 67%;
  - **gemini-3.1-flash-lite (the old default): 58%**.

  Strong tier on 6 tasks: gpt-6-sol 100% at 14 s ($0.015); sonnet-5 100% at 17 s ($0.059); opus-5.5 100% at 50 s ($0.19). The single-turn probe had earlier ruled out nemotron-3.5-lightning and mimo-v2.6-flash at 13–18 s per turn. Control arm pending.

### P1: contract and snapshot v2
- **Text-order bug fixed.** Feed rows now read actor first, one record per line ("14:09 UTC HL · Hanna Lindqvist revoked API key 'deploy-bot'").
- **Stable node keys** (WeakMap). Ids survive re-renders and are remapped by fingerprint only when the document changes.
- **Records and collections**, with columns and a `next` pager (orders, blog, load-more, activity and shipments all detected), plus **latent** elements with their reveal trigger (faq details, combobox listbox).
- **`changes_since` diffs by node key**, so repeated strings are no longer dropped.
- **Contract:**
  - I1: precise commands never fall into the fuzzy loop. An ambiguous name gets one narrow Choice among its candidates; anything else fails with the reason and the closest candidates.
  - I2: kind checks.
  - I3: every quoted literal must be used.
  - I4: one value per field.
  - I7: license Λ, a verb lexicon with stemming and synonym families.
- **Grammar fix.** `open the details of "X"` is no longer mistaken for a precise target.
- Scripted: 24/24 on cdp and 24/24 on camofox.

### P2: step engine, first results (offline, 258 decisions)
- **Parity mode** emits exactly the legacy questions: 49/49 identical decisions.
- **Code beats model on values.** Type-aware value spans (number fields get digits, email fields need `@`), number words → digits, and stripped possessives cut legacy's harmful commissions from **4.7% to 1.2%** and raised exact accuracy from 95.3% to 97.3%, at zero extra rounds or tokens.
- **The first dvm gates were miscalibrated.** τ(R2)=0.92 with a verify gate escalated correct Submit, Delete and Reject clicks. A too-narrow license lexicon ("Turn down all cookies" didn't license Reject all) caused more. After recalibrating (commit by default, block on a strong VERIFY "no"; stemmed synonyms), the dvm is within noise of legacy on this dataset: exact 95.7% vs 97.3%, commission 1.9% vs 1.2%, 0 escalations, +9% tokens.
- **Why:** this dataset comes from passing legacy runs plus paraphrases, so it has almost no risky wrong clicks for gates to catch. Harder cases are needed (hard-suite scripted steps, hand-made confusions, the held-out suite) before the gates can prove their worth.
- **Scoring fixes:**
  - Omissions (a deferred click) are separated from commissions (a wrong click or value).
  - Macros that correctly finish the next step early ("ahead of gold") are no longer counted as mistakes.

### P3: data layer (first version)
- `collect(what, where, op, of, by, pages)` pages through the collection's detected pager (or load-more), deduplicating rows.
- In **one** speculative Jev round it asks, for each condition, which column it tests, and for each (condition, column) pair, which distinct value it selects. It also asks which columns `of` and `by` refer to.
- Code then filters exactly, parses money and numbers, and computes count, sum, min, max or argmax, grouped when asked. It returns verbatim evidence rows.
- **Oracle:** a requested name that is literally a column skips its question.
- **Literal reading struck again:** "which column holds `quantity`" bound "total" to the **Items** column. The neutral wording ("contains the `wanted` values") fixed it.
- orders_refund_total → **585.10** (5 pages, 72 rows, exact); shipments_delayed_rotterdam → **6** (6 pages, 110 rows). Both arms had failed these through LLM arithmetic.

### P4: goal runtime (first version)
- `do(goal, steps?)`: without steps, the decision loop runs on the whole goal (step budget 15). With steps, each is an instruction string, `{collect…}` or `{expect…}`, with `{var}` substitution.
- **Clause gate:** the goal is split into clauses in code, then one Noul per clause is asked in a single request; `done` is reported only when every clause passes, otherwise `incomplete` with the unmet clauses.
- New `goal` bench arm, whose planner prompt says: call `do`, and use `collect` steps for data. `do` is also exposed over MCP.
- **Smoke test** (mercury-2.5, strict): orders 4.6 s / 3 turns, shipments 7.4 s / 3 turns, parcelwise 15.2 s / 12 turns; all PASS. For parcelwise the planner used `collect` (count by account → Halvorsen Freight AS) rather than `do`.

### Hedging, re-measured with low machine load (258 decisions, 2 passes each)
| policy | Jev requests | p50 | p95 |
|---|---|---|---|
| always hedge=2 | 516 | 279–285 ms | 441–473 ms |
| delayed, backup at p75 | 310–334 | 257–282 ms | 460–484 ms |
| delayed, backup at p50 | 373–394 | 298–306 ms | 504–520 ms |
| **no hedge** | **258** | 270–284 ms | **416–418 ms** |

At this load a duplicate request adds provider load without cutting the tail. The earlier −11% hedge win was measured on a noisier setup. **The default is now `hedge=1`**: half the requests and tokens, and p95 no worse. Delayed hedging (`hedge=2 hedge_q=0.75`) stays available as an option.

### P1 result: paired comparison, same tasks and models, strict checks
| suite | models | pre-P1 | P1 | discordant (pre-only / P1-only) | McNemar p | median wall (P1/pre) |
|---|---|---|---|---|---|---|
| component (6 tasks) | 7 cheap | 92.9% | **97.6%** | 1 / 3 | 0.63 | 1.02× |
| hard (6 tasks) | 7 cheap | 78.6% | **92.9%** | 2 / 8 | 0.11 | **0.76×** |
| component (3) | 3 strong | 100% | 100% | 0 / 0 | – | 0.86× |
| hard (3) | 3 strong | 100% | 100% | 0 / 0 | – | **0.76×** |

The direction is consistent (11 flips toward P1, 3 against) and the hard suite runs a quarter faster. It isn't yet significant on n=42 per suite; the main comparison adds wordings for more samples.

### P2 result on the full decision dataset (575 decisions, offline, live Jev)
| set | engine | exact | commission (harmful) | escalated | rounds/decision |
|---|---|---|---|---|---|
| component scripted + paraphrases (258) | legacy | 97.3% | 1.2% | 0% | 1.00 |
| | dvm | 95.7% | 1.9% | 0% | 1.02 |
| hard scripted (65) | legacy | 100% | 0% | 0% | 1.00 |
| | dvm | 100% | 0% | 0% | 1.00 |
| hard paraphrases (252) | legacy | 96.4% | 0.8% | 0% | 1.00 |
| | dvm | 96.4% | **0.4%** | 0.8% | 1.01 |
| **all 323 non-paraphrase + component para** | legacy | 96.0% | 1.5% | 0.3% | 1.00 |
| | dvm | **96.6%** | **1.2%** | 0.6% | 1.02 |

**Reading:**
- The formal engine (license, risk-tiered commit, same-request VERIFY gates, one refine round, escalation) holds accuracy and trades a few harmful commissions for a few escalations, at about 1.02 rounds per decision.
- The dataset still under-represents the cases the gates exist for: licensed-but-wrong risky clicks. Nearly every recorded decision is a correct commit from a passing run.
- **The big measured wins came from moving decisions into code and fixing observation:**
  - type-aware value spans cut commissions from 4.7% to 1.2%;
  - the contract (I1–I7) and stable keys;
  - record rendering fixed the feed misattribution;
  - `collect` made aggregation exact.

  Jev now makes the judgements code can't make, such as which column a condition means, and each such decision is cheap.

## Main comparison v1 (strict; mercury-2.5 + step-3.7-flash × 3 goal wordings), interim
Hard suite (14 tasks, 84 runs per arm; control 74 so far):

| arm | pass | p50 wall | LLM turns | $/task |
|---|---|---|---|---|
| control (chrome-devtools-mcp) | 89.2% | 18.4 s | 10.9 | $0.0052 |
| **experiment (usebrowser act/run/collect, P1)** | **96.4%** | **7.6 s** | **5.7** | **$0.0020** |
| goal (v1: `do` alongside act/run) | 90.5% | 9.0 s | 5.3 | $0.0022 |

Paired control vs experiment: 89.2% vs 95.9%, discordant 3/8, p=0.23, **wall 0.45×**. This reverses the original benchmark (usebrowser was 3–6 passes behind), now under strict checks.

**The goal arm didn't help in v1, because planners ignored `do`.** Only 14/80 component and 20/63 hard runs started with it; they drove act/run by id instead. Its failures still exposed real tool bugs:
- **`observe` padding.** 4 real matches were listed after 60 zero-score elements in document order, so the planner concluded "Zelda" wasn't on the page. It now returns only matches, best first, or says nothing matched.
- **Values.** With any quoted text in the instruction, unquoted numbers ("quantity 2") weren't value candidates. Rewriting ids injected quote characters, which then became "literals".
- **License gaps.** "Notify me" (on an out-of-stock item), Message and Call weren't commit actions. They are now R2 with their own verb families.
- **Verification steps acted.** "verify message sent" clicked a Message button. Steps starting verify/check/make sure/ensure now run as read-only expectations.
- **Record grounding (new rule).** A commit on a per-row button that repeats across rows must be on a row the step or goal refers to; otherwise escalate "which row?". This fixed "then click Send" messaging the first row.
- **Impossible gate.** A same-request Noul asks "does the page show the request is unavailable?". At ≥0.85, `do` returns `impossible` with evidence instead of exploring.
- **Money.** `collect` sums keep currency formatting ("$585.10", not 585.1).
- **No-feedback actions.** When actions ran but the page shows no confirmation, `do` reports `done_unverified` rather than `incomplete`. This stops retry storms that repeat actions.
- **Goal arm toolset.** Now `do` + collect/read/extract only, so the planner delegates. Corrections go through `do` steps, which can be precise commands.

Dev-build spot checks: lamp, orders, shipments 6/6; product_variant, checkout, spa, datepicker 8/8; biglist 5/6 (was 2/6), mostly in 2 LLM turns. Scripted regression: 24/24 and 14/14 on dvm.

### Decision engines with all post-v1 fixes (575 labelled decisions, offline)
| engine | exact | commission (harmful) | escalated | rounds/decision |
|---|---|---|---|---|
| legacy | 96.5% | 1.0% | 0.2% | 1.00 |
| **dvm** | **96.7%** | **0.5%** | 0.7% | 1.02 |

## Main comparison v1: final (strict; cheap = mercury-2.5 + step-3.7-flash × 3 wordings; strong = gpt-6-sol, canonical)
| suite | arm | n | pass | p50 wall | LLM turns | $/task |
|---|---|---|---|---|---|---|
| component (24) | control (chrome-devtools-mcp) | 138 | 93.5% | 7.2 s | 6.3 | $0.0056 |
| | experiment (usebrowser, P1) | 138 | 93.5% | **3.3 s** | 3.6 | **$0.0010** |
| | goal (v1) | 138 | 90.6% | 4.0 s | 3.3 | $0.0015 |
| hard (14) | control | 84 | 90.5% | 18.1 s | 10.6 | $0.0053 |
| | **experiment** | 84 | **96.4%** | **7.6 s** | 5.7 | **$0.0020** |
| | goal (v1) | 84 | 90.5% | 9.0 s | 5.3 | $0.0022 |
| hard, strong | control | 14 | 85.7% | 77.4 s | 10.1 | $0.0404 |
| | **experiment** | 14 | **100%** | **11.5 s** | 4.5 | $0.0131 |
| | goal (v1) | 14 | 92.9% | 9.3 s | 3.4 | $0.0127 |

Paired control vs experiment:
- component: equal pass rate (6/6 discordant), wall 0.54×;
- hard: +5.9 points (3/8 discordant, p=0.23), wall 0.45×;
- strong: +14 points (0/2), **wall 0.19×** (gpt-6-sol spends ~77 s per hard task driving chrome-devtools-mcp).

**Bottom line for v1.** Under strict checks and with the contract fixes, usebrowser now matches or beats chrome-devtools-mcp on pass rate in every cell (the original benchmark had it 3–6 passes behind). It is 2–5× faster and 3–5× cheaper. The goal arm is pending v2 with the post-v1 fixes.

### Tests added for the decision VM
- `dvm::round1` is pure: it builds round 1 without network. The `Oracle` trait lets the decision rules run against canned answers.
- **Golden parity:** parity mode emits exactly `decide::build`'s question batch on every recorded observation.
- **Replay rule tests:**
  - an unlicensed wishlist commit escalates, while the licensed add-to-cart commits;
  - a per-row "Message" without a referent escalates ("which row?"), while a referent row commits;
  - the impossible gate stops before any action;
  - a destructive commit needs VERIFY: a clear VERIFY "no" with no refine budget escalates.
- 21 tests total across both crates.

### Fix: snapshot cache vs property changes
The snapshot cache is keyed on the MutationObserver version, but property changes (a checkbox's `checked`, a typed `value`) aren't DOM mutations. So after "check X", the next snapshot was the cached "unchecked" one, and a second "check X" toggled the box back off. This showed up as login_noquote failing with `remember=off` in 3 goal-arm runs.

Input, change, click, keyup and reset events now bump the version. Regression after the fix:
- component, legacy/cdp: 24/24
- component, dvm/cdp: 24/24
- component, dvm/camofox: 23/24
- hard, dvm/cdp: 13/14 (the known session-expiry flake)

The first v2 attempt was discarded and rerun on the fixed binary.

## Test: Laya (open-weights System One model) as the decider, 2026-09-23
[convaiinnovations/laya](https://huggingface.co/convaiinnovations/laya) 0.3.11 is an Apache-2.0 encoder decision model: ModernBERT-large / mmBERT-base plus an option-marker head. Its `laya-serve` speaks Jev's `/v1/systemone`, so usebrowser used it with no code changes (`TYPESAFE_BASE_URL`, `TYPESAFE_API_KEY`, `UB_JEV_MODEL=<checkpoint>`).

**Setup:**
- Served locally on the M4 Pro GPU (MPS), bound to 127.0.0.1 with an API key.
- Token budget raised with a start hook (max_len 4096, head_max_len 1024). At Laya's defaults (512–1024 context, 192–256 option tokens), our 64-option click questions raise "options exceed head_max_len" and page states are truncated.
- New knob `gates_as_choice`: done/verify/impossible are asked as A/B Choices, the workaround Laya's README gives for `noul` answers that follow their labels. With plain Nouls, Laya's done gate sat at about 0.99, stopping 46/52 steps before acting.

**Offline, the same 52 recorded decisions** (gold = actions from strictly-passing Jev runs, so this favours Jev):

| decider | exact | harmful (wrong click/value) | p50 | p95 |
|---|---|---|---|---|
| Jev 1.13 (OpenRouter) | 100% | 0% | 291 ms | 380 ms |
| Laya typed-decisions | 44.2% | 38.5% | 402 ms | 1601 ms |
| Laya english | 38.5% | 28.8% | 405 ms | 1541 ms |
| Laya multilingual | 25.0% | 51.9% | 158 ms | 783 ms |

**End to end, scripted component suite (strict outcome checks, no gold bias).** Jev passes 24/24.
- Laya typed-decisions with the legacy engine: **3/24**, with side effects in 7 scenarios (wrong-value logins, search loops, a follow, a settings save, a consent click).
- Laya typed-decisions with the dvm engine: **1/24**, with 17 escalations. Side effects dropped to 4 scenarios (logins and searches): the license, grounding, VERIFY and impossible gates turned a weak decider's mistakes into refusals.

**Verdict:** zero-shot, Laya is not usable for browser decisions, and on our 2–3k-token states it isn't faster either. The 33 ms claim is for short texts on a T4. Laya scores each question as its own state+options sequence, so long states are re-encoded per question. This matches its README ("a fast base to specialise, not a zero-shot decision engine").

**Possible route:** fine-tune typed-decisions on our decision dataset, with Jev as the teacher (575 labelled decisions, more via `ub bench --record` and `ub decisions paraphrase`). The eval harness would measure it as is.

**VM gaps found:**
- "Sign in" isn't a commit in the license lexicon.
- The quoted-literal check (I3) runs after the commit click rather than before it, so a submit with missing values still happens.

## v2 results and held-out baseline, 2026-09-23

**Held-out (40 unseen tasks, mercury-2.5 + step-3.7-flash, strict):**

| arm | pass | p50 |
|---|---|---|
| control (LLM + chrome-devtools-mcp) | 66/80 | 18.6 s |
| experiment (LLM + act/run) | 68/80 | 10–13 s |
| goal (LLM + `do`) | 57/80 | 12–22 s |

`do` generalized worst. It averaged 7.8 planner turns and 18 Jev calls per passing task. Failures:
- 9 wrong field values;
- 6 turn-budget flails (console tree navigation);
- 3 provider outages (a 502, plus about 200 s hangs on an empty body);
- 2 unrequested mutations.

The strong-planner rows of `compare_v2.sh` are void: OpenRouter returned 429 for gpt-6-sol upstream. From here on held-out-1 counts as tuning data. The blind gate is `bench/heldout2.toml`, written by an isolated agent that never saw engine code or results.

## v3: agent mode, 2026-09-24

**Why agent mode.** In toolset mode the outer LLM's own turns cap the speedup:
- 20 of 31 race tasks were exactly delegate + finish, about 2.8 s at the median, against control's 7.3 s median;
- that is at most about 2.6× even with an instant engine, and about 7× with one turn.

So usebrowser now also takes the task itself (`ub bench --mode agent`, `ub race --right agent`):
- **At t=0**, one Jev request classifies the task (question? needs data first?) while the engine is already deciding. The engine sits behind a gate: nothing executes before classification, and no R2/R3 commit runs until it is known that no data program is needed.
- **Plain action tasks** make no LLM call at all; the answer is rendered by code.
- **Data and question tasks** go to the LLM: a one-shot compile (v3a) or the adaptive goal-toolset planner loop (`UB_PROGRAM_PATH=planner`, v3 held-out runs).
- **When the engine fails**, the LLM takes over (goal toolset, low reasoning effort). If nothing was committed, it restarts from the start page.

**Other changes, all kept:**
- Planner LLM clients are shared and pre-warmed per model, for all arms; before this every task paid a cold TLS handshake.
- LLM retries: 45 s request timeout, 5 attempts with exponential backoff, and empty bodies are retried.
- `bench --jobs N` runs parallel workers, each with its own browser and fixture server.
- Single and typographic quotes now parse as precise commands. Before, the planner's `type 'x' into e2` fell into the Jev loop: 6 extra rounds on checkout.
- Stale-element recovery: when a fill re-renders the list below it, the planned click re-resolves by fingerprint (role, name, context and record label, taken only if unique).
- The compile uses `reasoning.effort=low`: the same program in 1.0 s instead of 2.4 s. mercury-2.5 spent about 1.1k reasoning tokens per compile by default.
- Commits are licensed by the user's task, not only by a program step's wording. deploy_retry was blocked because "confirm" doesn't say "deploy".
- `data::number` takes the first numeric token ("16 GB LPDDR5X" had parsed as 165). Comparisons are unit-aware.

**v3a, race suites (37 tasks, mercury-2.5, arms run concurrently):**

| arm | pass | p50 | LLM calls/task | paired speedup vs control (geo-mean) |
|---|---|---|---|---|
| control | 89.2% | 7.5 s | 8.9 | — |
| goal | 91.9% | 4.2 s | 2.9 | 1.78× |
| agent | 97.3% | 1.5 s | 1.1 | 3.68× (median ratio 0.21) |
| vm (no LLM) | 64.9% | 1.2 s | 0 | 6.68× on its passes |

**h3a, held-out-1 (40 tasks, mercury-2.5):** control 80.0% (p50 16.9 s), goal 77.5% (13.1 s), agent **50.0%** (7.4 s). Agent mode's failures:
- **Engine-only premature done (5).** A form was filled but its submit never clicked, or a confirmation dialog was left open, and the clause gate accepted it.
- **Compile-only (4).** A wrong one-shot program executed confidently, e.g. "0 matched → 0 tickets".
- **Fallback (11).** The LLM took over from the engine's half-finished state; two of these were console flails.

**Fixes after h3a:**
- **Pending-commit guard** (code). Done is refused on whole-goal runs when an open dialog offers a licensed commit, or when fields changed after the last commit and a licensed commit control is still on the page.
- **Pre-commit form audit** (`audit=on`, one Jev round before R2/R3 clicks, after the fills). It is a contradiction check: does any field, or the summary on a review page, contradict the task, or turn on something the task didn't ask for? Values meant for later steps don't count. On a conflict it re-decides with a note instead of submitting. The first version asked about completeness and false-blocked multi-part tasks (cookie_banner, biglist). Field-less confirmation dialogs are skipped. A blocked click no longer counts as a repeat.
- **Click at the label point.** A click goes to the middle of the element's own first line of text, skipping nested lists, and is dispatched on the deepest element there, as a real click would be. JS `el.click()` on a tree item's `li` had missed handlers bound to its label. console_iam_viewer went from a 52.7 s turn-budget failure to a PASS in 8.6 s; console_guest_owner_impossible from 32.9 s FAIL to PASS in 8.1 s. No component-suite regression.
- **Planner path for data/question tasks, and reset before fallback.**

**Held-out-1 variants:**

| run | agent pass |
|---|---|
| h3b (guard, completeness audit, compile path) | 57.5% |
| h3c (+ planner path + reset) | 73% at 30/40 |
- h3c: 70.0% final.
- h3d (+ click fix, contradiction audit): 65% (26/40, single wording; noisy).
- **h3e (planner path at default reasoning effort, 2 wordings): 82.5%.** The low-effort planner lost about 12 points. Our planner turns now use default effort.
- **v3c (control at low effort, race suites):** 83.8% at p50 6.7 s, against 89.2% at 7.5 s at default effort. Control at default effort stays the baseline.

**Later fixes, each checked on its failing tasks and kept:**
- **Dialog answering.** When a run would stop with a dialog open, one Jev Choice picks the button the task calls for ("Don't send" for "don't send updates").
- **Modal detection.** Take the topmost *visible* `[aria-modal]`/`alertdialog`/`dialog:modal`. `querySelector` had picked a hidden editor dialog, so the snapshot reported no modal while a confirmation was on screen.
- **Audit.** A conflict found twice escalates instead of being submitted. Review pages name the conflicting summary line.
- **Final-exit guard.** A predicted-final click also stops when the form is still unsubmitted.
- **"impossible"** is final only for single-part tasks.
- **String data ops** ("collect X where …", "count …", "read", "extract …") run as data ops.
- **h3f, all of the above, paired, 80 runs per arm: agent 81.2% vs control 86.2%** (McNemar p=0.48), p50 14.9 s vs 17.3 s.
- **Generic commit license.** A task that asks for a change licenses Save/Submit/Update/Apply/Confirm. Specific commits (send, delete, pay) still need their own verb, and R3 is never generic. "not permitted" had been the top reason the engine escalated (7 of 26).
- **Typeahead reveal.** A latent option whose revealer is a text input is revealed by typing the words it shares with the task, then followed to its re-rendered node. drive_share_commenter went from a 162 s turn-budget failure to a 7.0 s PASS.
- **`steps_done`.** A program whose steps all ran reports what remains instead of "done_unverified", which planners had read as failure and retried: 44 → 8 such `do` results.
- **Near-miss names in precise commands** get one narrow Jev round among the 5 closest candidates. It only maps the target.
- **h3g, all of the above: agent 85.0% vs control 86.2%** (McNemar p=1.0, parity), p50 11.5 s vs 17.3 s, 1.59× geo-mean speedup on both-pass.

| h3g path | n | pass | agent p50 | control p50 on the same tasks |
|---|---|---|---|---|
| action, engine only | 16 | 13 | 3.3 s | 13.7 s |
| action, engine → LLM | 26 | 20 | 15.7 s | 24.9 s |
| data / question, planner | 38 | 35 | 13.1 s | 14.7 s |

**Since h3g:**
- **Direct answers in agent mode.** `do` and `collect` take an `answer` template; a finished call with every hole filled ends the loop without a `finish` turn. orders_refund_total and shipments_delayed now take 1 LLM call.
- **Whole-goal step budget 15 → 24.** "Step budget" was the top engine failure (6/26), on long multi-page forms.
- **Audit and dialog answering check against the user's task** in agent mode, not the planner's rewritten sub-goal.
- **Defaults:** `audit=on`; planner path for data/question tasks (`UB_PROGRAM_PATH=compile` restores the one-shot compile); default reasoning effort for our planner turns.

**Measured and reverted:**
- **Direct answers** (a filled `answer` template on `do`/`collect` ends the loop). h3h scored 77.5% vs h3g's 85.0% (12 lost, 6 won). The losses included wrong `collect` results returned unchecked: "4 tickets" and "117.00" came straight from a mis-bound collect. The p50 didn't improve either (12.6 s vs 11.5 s).
- **Audit checks against the user's whole task** instead of the `do` call's goal. h3i scored 73.75%. Audit blocks tripled (93 → 280) because intermediate commits inside planner programs looked contradictory to the whole task: 48–71 blocks per run on vet_slot_conflict, drive_share and stays_cheapest, which then ran out of turns. The audit is back on the `do` goal. Dialog answering keeps the user's task: dialogs are rare and need its context ("don't send updates").

**Kept, each verified on its failing tasks:**
- **Swallowed commits.** A popup that opens on a timer and makes the page ignore clicks swallowed "Confirm hold" in library_hold. After a dialog is dismissed with a non-commit button, the engine checks whether the last commit button is still there and enabled; if so the form counts as unsubmitted again.
- **The pending-commit guard names the right control:** the swallowed commit, else the last licensed commit in page order. A form's submit follows its fields; the button that opened the form comes before them.
- **A click that opens a new form**, with a licensed submit waiting, is never treated as the final action.
- **Agent mode no longer accepts `done_unverified` as final.** The clause check was right and the answer "Done" was wrong. The LLM now looks. library_hold went to 4/4 engine-only (4.6–6.5 s vs control's 20.2 s).
- **`collect` binds conditions to value sets.** One Jev yes/no per distinct value of the bound column, in one extra request, and rows are kept by membership. Binding one exact value undercounted multi-valued cells ("tags include refund" matched only "hardware refund").
- **The task classifier is accurate:** 2/89 action tasks routed as questions, 0/28 questions routed as actions.
- **Audit cost:** the engine-only race median rose from 1.1 s to 1.9 s (+0.9 Jev calls per task, plus a full settle after fills). The pre-audit settle is now light, 200 ms cap and no timer wait.

**Audit A/B (held-out, 80 runs each, same build, run concurrently):**

| variant | pass | p50 |
|---|---|---|
| audit on (τ 0.5) | 86.2% | 11.8 s |
| audit on (τ 0.3) | 85.0% | 12.2 s |
| audit off | 83.8% | 11.0 s |
| control (h3f) | 86.2% | 17.3 s |

Not significant (on vs off p=0.75). The audit stays on because it prevents wrong submissions.

h4on paths:

| path | n | pass | agent total | control total |
|---|---|---|---|---|
| action, engine only | 14 | 14 | 54 s | 187 s (3.5×) |
| action, engine → LLM | 28 | 21 | 698 s | 751 s |
| data/question, planner | 38 | 34 | 800 s | 770 s |

Whenever the LLM loops, agent mode runs at control's speed.

**Kept:**
- **Same-origin iframe changes invalidate the snapshot cache.** The observer and click/input listeners are attached to each frame document the snapshot walks. Otherwise opening a menu inside an iframe left a stale "closed" snapshot, and the engine toggled the menu shut again and again. desk_reassign_double_charge went from 0/4 to 3/4.
- **Loop detector** over the action history: 3 identical actions in a row, or A-B-A-B. It stops with the page's alert text. Each click changes something small (a message, a timestamp), so the page-version guard missed all of these, and "step budget" failures were loops that burned the whole budget: "Find my policy" ×8, Back/Review alternating.
- **Filter/search controls are R0** ("Apply filters", "Update results"). Read-only questions couldn't filter because "apply"/"update" count as commits.
- **In agent mode the user's task also licenses commits.** A planner's sub-goal may not repeat the user's verb.

**Measured and reverted:**
- **Sending `done_unverified` to the LLM in agent mode.** On the race suite, biglist went from 0.7 s to 10.5 s, confirm_delete from 1.0 s to 6.8 s, invoice_row from 0.8 s to 6.2 s. The actions were right; those pages just show no confirmation. The library case this was for is fixed by swallowed-commit detection.

**Tuned:**
- `audit_tau` 0.5 → 0.4. Borderline blocks near p=0.45 were false: settings the task doesn't mention, left at their defaults.

**Race suite:**

| run | pass | p50 | vs control (v3a) |
|---|---|---|---|
| v3e | 97.3% | 3.6 s | — |
| v3f (after the revert and τ=0.4) | 97.3% | 2.1 s | 3.3× median ratio, 2.8× geo-mean |
| v3a (no audit, no guards) | 97.3% | 1.5 s | — |

The accuracy work costs a little speed on tuned tasks and is worth +35 points on unseen ones.

**Held-out drift:** h5 83.8%, h6 82.5%, both within noise of h4on's 86.2%. The h4on and h6 binaries are being re-run as replicates to tell a regression from noise.

**New knob `clause_skip`** (default on): a single-clause goal that ended on a commit the audit passed with p ≥ 0.8, where the page changed and shows no error, skips the final clause-check round.

**Noise check, 2026-09-24.**
- h7 and h7r (the same binary, run twice) scored 81.2% and 80.0%. h7m (planner turns after the first at low effort) scored 80.0% at the same speed, so it was not adopted.
- All three looked below h4on's 86.2%. Bisection on the tasks that "regressed" (2 wordings × 2 repeats, h4on / h5 / h6 / current binaries): **8/16, 13/16, 12/16, 11/16.** The h4on binary did worst on the very tasks it had passed.
- Conclusion: per-task outcomes on LLM-driven paths vary a lot, so an 80-run held-out score carries about ±4 points. No regression was established. The current build sits around 82–84%, statistically at parity with control (86.2%).

**Also this round:**
- **Near-duplicate row grounding.** A repeated per-row commit must be on the row that best matches the instruction; exact whole matches of its strong literals (quoted text, emails, ids with digits) outweigh shared words. This targets maren.lund@ vs maren.lundqvist@.
- **Loop detector counts only actions without progress** (no new rows, no address not visited before), so "Load more" and "Next" are never loops.
- **Tried and reverted: one re-decision on a stuck loop.** It was slower (21–29 s vs 17 s) and didn't fix the case: the page needed a value reformatted, and Jev only picks from the task's spans.
- **CDP calls time out after 30 s**, and a bench worker whose browser died relaunches it. A Chrome auto-update had killed every browser mid-run and left the harness hanging.

**Fewer serial Jev rounds, 2026-09-24.**
- Jev latency is about 300 ms per call and barely depends on request size (1.2k tokens: 324 ms; larger: 389 ms). The lever is the number of serial rounds.
- On an idle machine (v4), engine-only race tasks averaged 4.8 Jev calls at a 397 ms median.
- **Cut:**
  - no audit for a per-row commit on a row with no fields ("Mark paid", "Message"); row grounding already decides which row;
  - the final clause check is skipped for single-clause goals that ended on such a row commit, or on a confident last click (p ≥ 0.9) that reached a new address with no error ("Sign in" → dashboard).
- **Result:** biglist 0.38 s (control 23.3 s), invoice_row 0.61 s (8.8 s), checkout 0.84 s (11.3 s), login 0.88 s (4.6 s).

**Clean race suite** (37 tasks, idle machine, mercury-2.5):

| run | pass | p50 | vs control |
|---|---|---|---|
| v4 control | 91.9% | 8.0 s | — |
| v4 agent | 97.3% | 2.4 s | 2.88× geo-mean |
| v5 agent (fewer rounds) | 97.3% | 1.7 s | 3.72× geo-mean, 0.26 median ratio |

In v5, 13 of 37 tasks are ≥5× faster and 4 are ≥10× (biglist 80×, pagination 35×, product_variant 31×, invoice_row 10.6×). Tasks that need LLM planner turns (parcelwise 8, laptop 6, wiki_tree 5) run at about control speed.

**Held-out** (h3f control 86.2%, p50 17.3 s):

| run | pass | p50 | vs control |
|---|---|---|---|
| h8, planner path (default) | 85.0% | 10.8 s | parity (p=1.0); 1.82× geo-mean, 1.93× median |
| h8c, compile path (empty `collect` falls back) | 85.0% | 12.4 s | 1.64× geo-mean |

The compile path falls back too often to save time on unseen tasks; the planner path stays the default.

**Which toolset the LLM gets in agent mode, 2026-09-24.**

Held-out by type (h8 vs control):

| type | runs | agent pass / p50 | control pass / p50 |
|---|---|---|---|
| action | 58 | 88%, 10.8 s | 84%, 20.3 s |
| question | 22 | 77%, 11.6 s | 91%, 12.3 s |

- **Full page text in the planner's first view** (questions): 77.3% vs 81.8%, no faster. Not adopted.
- **Fine-grained toolset (act/run/read with precise ids) for questions:** 86.4% at p50 7.6 s vs 81.8% at 10.1 s (44 paired runs).
- **Fine-grained toolset on every LLM path** (planner tasks and engine fallbacks), h9x: **91.2%** at p50 9.3 s. Against control (86.2%, 17.3 s): 8 won / 4 lost, **2.33× geo-mean speedup**, 2.13× median ratio. Against questions-only (h9, 83.8%): 8/2, p=0.11.
- Consistent with v2: when an LLM must drive, precise element-level tools generalize better than delegating whole goals to `do`. The split that works is the engine taking the task at t=0 and the LLM, with fine-grained tools, taking over only where the engine can't finish.
- **Default now.** `UB_PLANNER_TOOLSET=goal` / `UB_FALLBACK=goal` restore the old behavior.

## Final gate: blind held-out-2, 2026-09-24

**Setup.** 40 tasks on 20 new apps, written by an isolated agent that never saw engine code or results. Both cheap planners, canonical wording plus one paraphrase, 160 runs per arm, arms run concurrently. Binary `ub-g2`, the final defaults.

| arm | pass | p50 | LLM turns | $/task |
|---|---|---|---|---|
| control (LLM + chrome-devtools-mcp) | 74.4% | 32.4 s | 19.0 | $0.0164 |
| goal (LLM + `do`) | 74.4% | 20.6 s | 10.5 | $0.0129 |
| **agent (usebrowser runs the task)** | **75.6%** | **15.3 s** | 9.7 | $0.0072 |

- **Agent vs control:** 30 won / 28 lost, McNemar p=0.90, i.e. parity. **2.14× geo-mean speedup** (0.49 median ratio), 2.3× cheaper.
- **By planner:**

| planner | agent | control | agent speedup |
|---|---|---|---|
| mercury-2.5 | 60/80 | 57/80 | 2.48× |
| step-3.7-flash | 61/80 | 62/80 | 1.88× |

- **By path:**

| agent path | runs | agent pass | control pass (same tasks) | speedup (geo-mean) |
|---|---|---|---|---|
| engine only | 29 | 14 | 23 | **9.46×** (p50 3.4 s vs 29.1 s) |
| with LLM | 131 | 107 | 96 | 1.86× |

  The engine-only path is too eager on unseen sites: 10 "done" and 4 "done_unverified" completions were wrong. This is the main remaining accuracy gap. On held-out-1, where the engine's guards were developed, the engine-only path was 14/14.
- **By category:**
  - **Agent ahead:** data 30 vs 25; aggregation 8 vs 4; sold-out 11 vs 8; session-expiry 4 vs 1; flaky-save 4 vs 2.
  - **Agent behind:** "only X where the UI defaults to X+Y" 5 vs 12; taken-name 0 vs 4; restraint 16 vs 20; navigation 25 vs 29.

**Validity caveats:**
1. The authoring agent was still fixing pages and 4 scenario definitions until 02:48, about 10 minutes into this run. Grading criteria load at start, while pages are served live, so some runs were graded against pre-fix expectations. All arms ran concurrently on the same files.
2. The re-run on the final files (g2b) is **void**. The OpenRouter key's spending limit ran out mid-run (`402 in_flight_budget_exhausted`), failing 118 control, 87 goal and 52 agent runs. Its 42 clean pairs (survivor-biased toward short tasks) point the same way: agent 90% vs control 81%, 2.04×.
3. `bench/summary.py` now excludes provider failures (402/429/5xx/decoding) from paired comparisons.

**Where 10× stands:**
- Reached where the engine finishes alone: 9.5× on the blind suite; 80×, 35× and 31× on biglist, pagination and product_variant.
- Not reached end to end. Any task that needs LLM turns runs at 1.9–2.5×, because each planner turn costs about 1–1.4 s. On unseen sites the engine can finish alone only if its completion claims become more reliable; the "only X" / restraint / taken-name categories show where.
- Next, once credits are available: re-run the gate on the final files, then attack engine-only false completions. Validating that needs a fresh held-out-3; held-out-2 has now been looked at.

## Round 2 (credits restored), 2026-09-24

**Protocol.** g2c re-runs the blind held-out-2 gate on the final suite files with the round-1 build. After that, held-out-2 is tuning data. A new blind held-out-3 (40 tasks on 20 more apps, isolated author, writing only to its own new paths) is the next gate.

**Held-out-2 failure analysis, round-1 build. Each fix is general; each is verified on its failing tasks.**
1. **Settle ignored "Saving…".** Engine-only runs declared done while the page still showed "Saving…" / "Renaming…". The fixture's save takes about 1.2 s, and settle doesn't wait for timers over 600 ms. Settle now keeps waiting (up to 8 s) while a visible button or status reads "<word>ing…" / "Please wait…", or anything is `aria-busy`. tax_dependant_childcare, repo_rename_fallback and router_port_forward went from 0/6 to 6/6 engine-only, at 3–4 s each.
2. **Search-as-you-type comboboxes.** Typing isn't choosing. After a combobox fill, the engine clicks the one suggestion whose name, or own label, is exactly the typed value. It never takes a longer name that merely starts with the value: "ci/build-docs" is not "ci/build". A value already picked into a combobox isn't typed again in that act; the input empties once the choice becomes a chip. repo_branch_protection canonical: 0/2 → 2/2 engine-only.
3. **Styled switches were invisible.** `<input type=checkbox role=switch>` hidden with opacity 0 inside a visible label was dropped by the snapshot. The styled-input rule covered only the checkbox and radio roles. camp_nico_single_week: 0/4 → 2/2. The "Full-Summer Pass" toggle was never shown to the LLM, which then ran out of turns.
4. **Native switches reported no state.** A checked `<input type=checkbox role=switch>` showed no "checked" flag, so check/uncheck compared against the wrong state. guests_add_household: 0/4 → 2/2. The pre-checked "Allow plus-one" is now seen and turned off.
5. **Links that start a flow are navigation (R1), not commits:** register, sign up, create, book, reserve, apply, checkout, start trial, invite, upload. "Register a camper" is a link to a form; the form's submit is the commit. Destructive and direct-commit verbs keep their class on links.
6. **`done_unverified` goes to the LLM again** in agent mode. On the blind suite all 4 such engine completions were wrong; on the tuned race suite only 2 correct ones end this way.
7. **Jev client** retries body read and decode failures and 504 (one blind-suite run died on "error decoding response").

**Clean blind gate g2c (final held-out-2 files, round-1 build + Jev retry fix):**

| arm | pass | p50 | turns | $/task |
|---|---|---|---|---|
| control | 73.1% | 30.5 s | 18.9 | $0.0158 |
| goal | 74.8% | 19.4 s | 9.9 | — |
| agent | 76.1% | 15.4 s | 10.1 | $0.0078 |

Agent vs control: p=0.60, **2.03× geo-mean**. This replicates g2 (75.6% vs 74.4%, 2.14×), so the first run's mid-run file change didn't distort it. **This is the last blind number for the round-1 build.**

**More general fixes (held-out-2 is now tuning data; each verified on its failing tasks):**
8. **The element listing puts revealed controls first**, by content fingerprint (role, name, context) new since the last action, and, while a modal is open, the elements not behind it. A dialog's buttons are usually last in the page and fell past the 60-element cap. mail_filter_new_only: 0/6 → 2/2; lms_notification_override: 0/2 → 2/2. Node keys can't be used for "new": apps that re-render the whole view give every element a new node.
9. **Stale elements are re-resolved before every action**, not only after an earlier action in the same plan. Async re-renders between snapshot and action caused 7 escalations.
10. **License:**
    - flow-starter links add submit, request, post and message ("Submit a meter reading" opens a form);
    - change verbs add make, enter, record, log, provide, input and put ("make X the primary driver" licenses "Save assignment");
    - **semantic license:** an R2 click the lexicon rejects is allowed when the same-request VERIFY is ≥ 0.85 AND the task's own verbs license no other commit control on the page. It covers paraphrases without the verb and adds no extra round. R3 is never licensed this way. Unit test: "add laptop to cart" still never licenses "Save to wishlist".

    florist_juniper_order and fleet_primary_driver_retry: 0 → 2/2.
11. **Follow-up questions after a submit.** New form fields that appear after the last commit click, with a licensed submit still present, mean the submission isn't through ("reading lower than last time? tick the rollover box"). Busy labels are now phrases starting with an -ing word ("Checking reading…"). Error alerts also match "…submit again", "check the…", "must…", "lower than…". energy_gas_rollover, paraphrase wording: engine-only 0 → PASS in 3.8 s (control 16.4 s).
12. **Commit evidence** replaces the final clause check for single-part goals when any of these ran and the page moved on with no error, nothing pending and nothing busy:
    - a commit this act;
    - a confident (p ≥ 0.8) final click;
    - an unaudited commit (a confirmation dialog, a per-row action).

    The clause check misjudged pages where the evidence is gone (a deleted file). confirm_delete 8.3 s → 0.5 s; invoice_row 6.8 s → 0.35 s.

**Measured and dropped:**
- **Batching hint in the planner prompt ("do everything visible in one call").** 90.0% vs 90.6%, turns 8.1 vs 8.5, p50 14.5 s vs 13.5 s: no gain. Per-turn latency floors at about 1.2 s regardless of prompt size (1267 vs 1318 ms by prompt-size half), so turn count, not context size, is the lever.
- **"Clicked button went disabled with the same label" as an in-progress signal.** It had no case it fixed (energy's label changes to "Checking reading…"), and it cost 1.5–8 s on apps that disable a button for good once done ("Mark paid").

**Round 3 (tuning numbers, optimistic):**

| suite | agent | control | agent speedup |
|---|---|---|---|
| held-out-2 | **90.6%** (p < 0.001) | 73.1% | 2.30× |
| held-out-1 | 91.2% | — | — |
| race | 97.3%, p50 1.3 s | 91.9% | 3.82× |

**Round 4** (stale re-resolve, license extensions, semantic license, follow-up-question guard, commit evidence):

| suite | pass | vs round 3 | speed |
|---|---|---|---|
| held-out-2 | 88.1% | 90.6%, p=0.45 | 2.33× vs control; LLM turns 8.5 → 7.6 |
| held-out-1 | 90.0% | 91.2%, p=1.0 | 2.40× vs control |
| race | 97.3% | same | **4.97× geo-mean** vs control, 0.18 median ratio (3.82× before) |

Accuracy is flat within noise and speed is up: commit evidence keeps simple tasks engine-only.

**Round 5.** On the user's go-ahead ("its ok if p90 calls out to small llm in cases where extra clarity is needed"):
13. **Clarify** (`clarify=<model>`, on in agent mode with the planner model; `UB_CLARIFY=0` turns it off).
    - When a decision escalates for uncertainty ("ambiguous: best guess", "no candidate", "nothing further", "not permitted" on an R2), one small-LLM call (low reasoning effort, about 0.8–1.1 s) picks among Jev's ranked candidates plus the best lexical matches, or says `done` / `none`. The engine then continues, instead of handing the whole task to the planner loop (about 10 turns, about 13 s).
    - Picks are still gated in code: R3 needs the task's own verb; R2 needs it, or no other commit control the task's verbs point to; the row grounding applies.
    - The calls count as LLM calls (turns and cost) in the report.
14. **Fill first on an uncertain click.** When the click is uncertain but the plan has field values, the fields are filled (typing commits nothing) and the step is decided again. A filled field often enables or explains the button.
15. **Confirmation text.** A field whose label or context says "Type UNLINK to confirm" / "type “acme/api” to confirm" is offered that token as a value (quoted, all-caps or path-like only). The destructive click itself stays license-gated.
16. **Lexicon:** unlink, disconnect and detach are R3 (with families). Expanders (an aria-expanded state, or a name starting with Expand/Collapse/Show/Hide) are R0 ("Expand checkout" is not a checkout).
17. **The R1 (navigation) threshold drops from 0.5 to 0.4.** On held-out, 15 of 24 "best guess" escalations were links at p 0.42–0.49, and most were right.
18. **The pending-submit guard also sees a submit that is disabled until a newly shown field is filled.**

transit_unlink_card, one run each:

| build | agent | LLM turns | control |
|---|---|---|---|
| before | 9.6 s | 5 | 13.7 s |
| after | **6.4 s**, engine-driven | 1 (the clarification) | 16.2 s |

In the new run the engine typed UNLINK, then made the confirm click at p=0.99.

**Round 5 results:**

| suite | round 5 | round 4 | notes |
|---|---|---|---|
| held-out-2 | 87.5% | 88.1% (p=1.0) | 2.37× vs control |
| held-out-1 | 92.5% | 90.0% (p=0.73) | 2.21× vs control |
| race | 97.3% | 97.3% | p50 1.1 s, 4.85× vs control |

- **Clarify doesn't earn its keep.** It fired in 40/160 held-out-2 runs (102 calls); the planner still took over in 32 of them; the 8 runs it finished alone were 4/8 right. It is now **off by default** (`UB_CLARIFY=1` enables it).
- **Kept:** fill-first, confirmation tokens, the R1 threshold of 0.4 and expanders as R0. They are cheap and principled, and all suites were flat or better.

## Site-shape cache, 2026-09-24

At the user's request, all learned information about a site's structure is cached, efficiently and never with identities or API calls (`crates/usebrowser/src/shape.rs`; design in the README):
- one JSON file per site: page templates plus hashed navigation edges;
- records only R0/R1 controls, never anything inside a record, with the task's literals, emails and digits masked everywhere;
- one store per directory shared by all sessions in a process, merged with the disk copy on every write;
- used through `Session::shape_navigate`: one Jev choice of the destination page among those reachable, then a verified replay of the path. It runs first in agent mode, and in `do` for whole goals.

**First test** (mail app, held-out-2): the recorded map has 7 templates and 6 edges in 1.5 KB. The first "use" run's destination pick was unsure (p=0.52): sibling settings pages had identical descriptions. Descriptions now include the selected tab, and `shape_tau` is 0.45 (a wrong jump only costs a navigation).

Benchmarks default to `shape=off`, so all numbers here are cold; `bench/warm.sh` measures cross-task transfer.

## Blind gate: held-out-3 (40 tasks on 20 new apps), 2026-09-24

Held-out-3 was written by an isolated agent that never saw engine code or results: 152/152 checks, and honest scripted solutions pass while 112 wrong paths fail. Final build, cold (`shape=off`), both cheap planners, canonical wording plus one paraphrase, 160 runs per arm, arms run concurrently.

| arm | pass | p50 | LLM turns | $/task |
|---|---|---|---|---|
| control (LLM + chrome-devtools-mcp) | 73.1% | 40.7 s | 20.1 | $0.0133 |
| goal (LLM + `do`) | 88.1% | 19.2 s | 8.1 | $0.0059 |
| **agent (usebrowser runs the task)** | **92.5%** | **16.6 s** | 7.8 | **$0.0044** |

- **Agent vs control:** 38 won / 7 lost, McNemar **p < 0.001**. **2.90× geo-mean speedup**, 0.36 median ratio, 3× cheaper. Both planners agree: mercury-2.5 72/80 vs 56/80 (3.14×); step-3.7-flash 76/80 vs 61/80 (2.71×).
- **By path:**

| path | runs | agent pass | control pass (same tasks) | speed |
|---|---|---|---|---|
| engine only | 32 | 31 | 27 | **8.47×** geo-mean (p50 2.9 s vs 35.5 s) |
| with LLM | 128 | 117 | 90 | 2.05× |

  Round 1 on the blind held-out-2 had engine-only at 14/29; the fixes to its false completions generalized.
- **By category:**

| category | agent | control |
|---|---|---|
| form | 44 | 21 |
| data | 32 | 26 |
| failure | 24 | 20 |
| restraint | 23 | 19 |
| navigation | **25** | **31** (the one category where control wins) |

- **Per task,** among the 110 both-pass tasks: 9 are ≥10× faster, 24 ≥5×, 68 ≥2×, and 12 slower.
- **Comparison with round 1's blind gate on held-out-2:** 76.1% vs 73.1% (parity) at 2.03× then; now 92.5% vs 73.1% (p<0.001) at 2.90×.

**Found through held-out-3's failures (post-gate, so the blind number stands as recorded):**
- **CDP replies with lone UTF-16 surrogates were silently dropped.**
  - Our page script cut strings by UTF-16 units and split an emoji's surrogate pair ("📁 2026 ▸ 📁 2025 📄 …" at 80 chars). Chrome escapes the lone half as `\ud83d`; `serde_json` rejects it; the reader dropped the message, and the caller waited 30 s. Every later call on that page did the same.
  - Node's JSON parser accepts lone surrogates, so chrome-devtools-mcp was unaffected.
  - Fix: surrogate-safe truncation in the page script, plus a reader that repairs unpaired `\uXXXX` surrogates to U+FFFD instead of dropping the reply (unit-tested, including escaped backslashes). Unparseable messages are now logged.
  - Impact: all 4 agent and all 4 goal runs of tenant_west_elevator_contractor on held-out-3 (post-hoc; the gate number isn't revised).
- **Diagnostics added along the way:**
  - CDP timeouts name the call that timed out;
  - `Runtime.evaluate` carries a 20 s termination timeout;
  - the page walk has node and time budgets, and skips nodes it has already visited.

**Site-shape transfer** (`bench/warm.sh`: learn from each app's first task, then run its second task with and without the map, paired):

| suite | no map | with map | notes |
|---|---|---|---|
| held-out-2 | 88.8%, p50 12.4 s | 91.2%, p50 14.9 s | McNemar p=0.63; 0.89× geo-mean (slower); engine-only completions 21 → 17 |
| held-out-3 | 89.9% | 92.4% | p=0.75; 1.04× geo-mean |

Accuracy is up on both suites but not significantly; speed is mixed. The cause of the slowdown: wizard controls (Continue/Next) had been recorded as navigation edges, so replaying them jumped into later wizard steps without earlier input. Also, one-hop jumps cost a Jev round and save one. Fixed: wizard-control names are never edges, and the map only jumps two or more hops. Re-measuring (w2b, w3b).

**Transfer re-measured** (wizard controls never edges, jumps of two or more hops only):

| suite | without map | with map | speed |
|---|---|---|---|
| held-out-3 | 92.5% | 93.8% (p=1.0) | 1.01× |
| held-out-2 | 91.2% | 90.0% (p=1.0) | 0.90× |

With the map, Jev calls per task go from 11.0 to 12.0, and LLM turns stay at 6.8. The median per-task delta is +0.4 s; the large swings are planner variance. The fixture apps are shallow (2–3 hops), and LLM turns, not navigation, dominate the time.

**Verdict:** the cache records by default (identity-free; 20 sites, 62 templates, 35 edges, 13.5 KB total), and acting on it (`shape=on`) is opt-in until a suite with deeper sites shows a gain.

## 2026-09-25: fab (renamed), agent CLI, MCP server, browser discovery

> The MCP server described in this entry was removed later; agents use the CLI and `fab --skill`.

The project is now **fast-agentic-browser**:
- the engine crate is `fab-core`;
- the binary crate is `fast-agentic-browser`, which builds the `fab` binary;
- `UB_*` environment variables are now `FAB_*`;
- the cache moved from `~/.cache/usebrowser` to `~/.cache/fab`, and site shapes are moved there on first use.

Commands and names in the entries above keep their old spelling.

**CDP backend rewrite.**
- One browser websocket, with a flattened session per tab (one session per target).
- New: tabs; attaching to a running browser (DevToolsActivePort, with ports 9222/9229 as a fallback); following `target=_blank` and `window.open` tabs (`--disable-popup-blocking`); answering JS dialogs at once; console and network capture; closing a persistent profile gracefully.
- Benchmarks run with capture off, headless, and with a temporary profile.

**Regression check** (Jev only), new build vs the last pre-rename binary (`ub-w3b`), run on the same machine:

| suite | result | per-scenario | p50, new vs old |
|---|---|---|---|
| scripted component suite | 24/24 | — | — |
| engine-only, component suite | 21/23 | identical | 831 vs 864 ms |
| engine-only, hard suite | 4/14 | identical | 2533 vs 2611 ms |

**Found while wiring the CLI.** Sessions and MCP ran the library's default engine, `legacy`. The speculative gate of agent mode exists only in the decision VM, so a question task went ahead with its speculative login and the planner then had to undo it. Sessions and MCP now default to `engine=dvm`, the engine every agent-mode result was measured with.

**LLM tail latency.** One stalled mercury-2.5 call took 46.7 s (a 45 s timeout, then a retry). Planner calls now race a second copy once they run past 3× the recent average (at least 6 s). Only slow calls are duplicated.

**Firefox over WebDriver BiDi** (`backend/bidi.rs`). The OS default browser on the dev machine is Firefox, so fab now drives it rather than falling back to Chrome. The backend does the same jobs as the CDP one:
- a profile fab owns, and `user.js` prefs that turn off first-run pages, updates and slow-script prompts;
- the snapshot script as a preload script;
- trusted input through `input.performActions`;
- tabs and popups (`originalOpener`), prompts, and `log`/`network` capture.

Same engine suites, Firefox 156 vs Chrome 153:

| suite | Chrome | Firefox | p50, Chrome vs Firefox |
|---|---|---|---|
| scripted | 24/24 | 24/24, identical per scenario | 601 vs 621 ms |
| engine-only, component | 21/23 | 21/23, identical per scenario | 831 vs 858 ms |
| engine-only, hard | 4/14 | 5/14 (`parcelwise_hold_overdue` also passes) | 2533 vs 2598 ms |

Parity, so sessions follow the default browser. Benchmarks pin Chrome, to stay comparable with every earlier result and with the chrome-devtools-mcp control.

**Fixed along the way.** After `back`, Firefox restores the whole login form (bfcache), so the engine only had to click Sign in. The literal check then looked for "alice" on the page after navigation and failed the step. A literal already held by a field before acting now counts as used.

## 2026-09-25 · Secrets: `login` and `fill` with values no model sees

**Goal.** Agents sign in and fill forms with the user's passwords, codes, cards and personal details, taken from their password manager (1Password, Bitwarden, macOS Keychain, or any `fab-secret-<name>` helper). No value may reach an LLM, Jev, output, logs or caches.

**The interface came from sampling, not design** (`bench/ergonomics/sample.py`, OpenRouter, $0.25 in all):
- **Round 1, unprompted:** 11 cheap models × 8 flows × 2 samples. The prompt said secrets live in the password manager and the model must not see them, and asked it to invent the call.
  - Logins were `{"url": …}` in 21/22 answers, and accounts were picked by `username`/`email`.
  - Form pages got `fields: {e3: value}`.
  - No model ever wrote a vault path (`op://…`). They named secrets in plain words, most often as `"{{…}}"`, then as `{"source": "password_manager", "name": …}` objects.
  - 5/11 models made up a password for signup, and several typed "John Doe / 123 Main St".
- **Round 2, real tool calls, scored:** 12 models, the same flows, with `"{{name}}"` vs `{"secret": "name"}`. Results: 78% vs 68%. Small models turned the object into a string, which hurt most on checkout (54% vs 29%) and API keys (71% vs 46%). `"{{name}}"` won.

**Built.**
- **`fab-core/src/secrets/`:** the plain-words name table, which maps names to the standard autofill field names (`one-time-code`, `cc-number`, `postal-code`, …). It also holds the four stores, site matching, approvals and the redactor.
- **The only way in is `Browser::fill`/`select`.** Values are typed as keystrokes, never inside an eval (debug logs print evals). Fields fab typed a secret into are masked by `snapshot.js`.
- **Redaction** covers snapshots (so Jev, planners, shapes and traces see only `••••`), every tool reply, live progress lines and errors, in raw, URL-encoded, JSON-escaped, HTML-escaped and base64 forms.
- **Site binding.** A login fills only on its saved site (registrable domain, shared-hosting suffixes kept apart, never https→http). Cards, identities and other items need the user's approval per site, via a system dialog or `fab secrets allow`.
- **MCP tools** `login` and `fill`, and **CLI** `fab login`, `fab fill k=v … --submit --save` and `fab secrets`.
- **`bench/secrets/e2e.sh`** runs a careless fixture site that logs the password, sends it in a URL and echoes it. The script checks sign-in with a one-time code, checkout (approval refused, then allowed), signup with a generated password saved first, an API key, refusal on another site, and refusal of `{{…}}` in `goto`/`eval`. It then greps all output and fab's files for the values: **PASS on Chrome and Firefox, no value anywhere.**

**Ergonomics gate** (fab's real definitions and server instructions, scored after fab's own parsing via `fab __normalize-fill`, 12 models × 3 samples):

| round | tuned tasks | held-out tasks |
|---|---|---|
| first cut | 84% | — |
| + fuller examples, "" means skip, `submit`/`save` inside `fields` | 89% | 54% (seen afterwards, then moved into the tuned set) |
| + name values after their field, a named item as fallback, fab-side key resolution | 87% (with those 2 tasks added) | 81% (seen afterwards, then moved in) |
| + JSON repair for `"{{x"}}`, a button in `fields` = click it, clearer no-login error | **91%** (12 tasks) | **76%** (2 fresh tasks, never tuned on) |

About 5% of all calls weren't tool calls (empty replies from gemini-2.5-flash-lite and nemotron-3-nano, and clarifying questions from gpt-5-nano). On held-out tasks, models most often write the bare field name (`{{PIN}}`, `{{password}}`) when the user named the saved item. fab can't infer that name, so its error says to include it, and one retry recovers.

**Not done.**
- High-level instructions with placeholders (`act 'log in as "{{username}}" …'`) aren't reliable: Jev didn't assign the opaque `"{{username}}"` to an Email field. Placeholders work in `fill`, `login` and precise commands, and the docs say so. The engine is unchanged.
- 1Password writes (`save`) went through `op item create` from stdin, but the live dry run timed out waiting for the desktop app's approval, so only the fixture store has exercised `store`. Bitwarden isn't installed here: its code is unit-tested on its JSON only.

**Regression.** Scripted component suite: 23/24. `load_more` stopped one Jev decision early, then passed 5/5 on rerun. Snapshot redaction is a no-op until a secret has been typed.

## 2026-09-26 · One step in words: the public surface cut to `do`

**Why.** fab exists so the agent hands over the browser work. The surface had
grown the other way (23 MCP tools, ~36 CLI commands: click, fill, press,
screenshot, console, eval…), inviting agents to drive the browser step by step.
No benchmark arm used any of it: they call the library (`Session::act/run`,
`autopilot::run`, the bench-internal planner tools).

**Surface now.**

| | before | after |
|---|---|---|
| MCP tools | 23 | 1: `do {step, url?}` |
| agent CLI | ~36 commands | `fab do "<step>" [--url U]` |
| setup | … | `sessions`, `close`, `mcp [--shared]`, `secrets`, `doctor`, `install`, `fab --skill [--install]` |
| harness | in `fab` | `fab-bench` (bench, race, decisions, compare, rescore, show, serve, oneshot) |

- **`do` is agent mode.** A step starting "open example.com …" opens it first. The reply is the answer, the time and LLM calls, then `now at:` with the title, address and page text (no element ids: nothing takes them).
- **Sign-in happens inside a step** (the step says "log in", or the page is a sign-in form) when a saved login exists and the step brings no credentials. This runs before agent mode, so `autopilot::run`, the benchmarked path, is unchanged.
- **Deleted backend code, −826 lines:** screenshots, hover, scroll, upload, key presses, history, tabs, response bodies, and the console/network log with its `capture` knob. Dialog answering and popup following stay.

**Parity** (the same machine, this build vs the one before):

| suite | before | after |
|---|---|---|
| vm, component | 21/23 | 21/23 |
| vm, hard | 4/14 | 4/14 |
| agent, component | 22/23 | 22/23 |
| agent, hard | 14/14 | 14/14 |
| scripted | 24/24 (last run) | 24/24 |

Every scenario had the same outcome. Timing looked mixed at first, with the hard suites slower, so the vm hard suite was rerun twice with the two builds alternating. The new build was slightly faster: p50 2.30 vs 2.36 s, sum 29–31 vs 32–33 s. The agent-mode deltas are in scenarios that wait on OpenRouter LLM calls.

**Secrets through `do`, and a bug the end-to-end run found.** With a card not yet approved, typing failed inside the engine. The LLM fallback then typed the placeholder *names* as text ("full name", "card number") and placed the order. A `{{country}}` select also got a guessed option. Now every `{{…}}` in a step is resolved before fab acts:
- a missing item or refused approval stops the step, with nothing typed and no fallback;
- plain values (name, address, country, username) go into the step as quoted text, which the engine matches to fields and options like any literal;
- concealed values stay placeholders until their field is filled.

`spans` treats `{{…}}` as a literal that fits any field type. That was also why `"{{username}}"` was never offered for an Email field. A step with `{{new password}}` saves the login under the quoted email before typing. `bench/secrets/e2e.sh` now runs through `fab do`: PASS on Chrome and Firefox, no leaks.

**README demo** (`bench/race-gif/`): `fab-bench race` recorded from inside both browsers with DevTools screenshots (no screen-recording permission needed), then composed with timers.
- The race's left browser and dashboard now always use Chrome (they had launched the user's default, Firefox).
- `--lead` holds both ready pages for 2 s before the clocks start.
- **The GIF uses a tennis-club re-skin** (`bench/demo.toml`, `fixtures/demo/tennis.html`) of the blind held-out-3 task `wineclub_customize_october`: the same page mechanics and the same kind of goal (two swaps among look-alike options, a date, save), with strict record checks.
  - fab alone: 3/3 PASS, p50 2.8 s, no LLM calls.
  - The race (mercury-2.5): chrome-devtools-mcp PASS in 35.0 s over 16 turns, fab PASS in 2.7 s (13×).
  - The original wine-club task raced the same way: 35.0 s vs 2.6 s.

**Found on the live Hacker News site (not fixed).** fab doesn't recognise HN's two-row story layout as a collection ("no named columns"), so aggregate questions fall back to reading or planning:
- "Which front-page story has the most comments?" answered 207 comments; the truth, from HN's HTML, was 725.
- "Stories over 300 points on the first 3 pages" found 15 of 16 (it missed the 941-point one), taking 16 s and 11 LLM calls.

The top-10 list is exact (6.8 s vs 3.5 s for chrome-devtools-mcp vs fab). Next: records for row-pair layouts like HN's, measured on live pages.

## 2026-09-26 · `fab do` as compile-to-line-script + run (not adopted as default)

`do` compiled the request with mercury-2.5 into a line script (spec D12, from the
format spike: 81-84% of cheap-model scripts capture the request) and ran it line by
line. Held-out-3, 40 tasks, mercury-2.5, jobs 4:

| arm | pass | p50 | cost |
|---|---|---|---|
| agent (today's `do`) | **35/40** | 9.8 s | $0.15 |
| script, lines = agent mode each | parcelwise only: wrong hold reason + a suspend (destructive) | | |
| script, action lines engine-only, scoped to the request | 6/40 | 2.7 s | $0.02 |
| + a no-op line counts as done (reverted) | 3/40 | 1.7 s | $0.02 |
| compile sees the page; a failed line hands the rest to agent mode | 34/40 | 15.5 s | $0.12 |

Scripts compiled without seeing the site are plans of guesses: "open the X site and
sign in" when it's open and there's no sign-in, guessed button names and
confirmation texts. Strict per-line execution stops at the first wrong guess (17/34
failures on line 1). Per-line agent mode improvises without the request's context
and committed wrong actions. With the page in the compile prompt and agent-mode
handoff, 34/40 runs handed off (29 passed); the script alone finished 3 (2 passed).
Compiled lines still name controls on pages the compiler hasn't seen. Handoff
matches agent mode's accuracy at +58% p50: no measured win from compiling first.

## 2026-09-26 · `fab do` as a control-flow program (IR) + VM (kept opt-in: `--program`)

Reframed: the program holds only logic known in advance (values carried, loops,
conditions); leaves are whole sub-goals decided on the live page (`do` = agent
mode, `read`, `test` = one Jev check). Syntax spike (10 cheap models, 24 dev + 12
held-out requests): end-delimited blocks chosen over labels/`br_if` and a Python
subset (judge pass ~48% vs ~42% Python, ~38% labels; Python lost 4-8% to syntax
errors; blocks parse leniently always). Without an example of a plain request as a
single `do`, programs spelled out clicks and looped where nothing repeats.

Held-out-3, 40 tasks, mercury-2.5:

| `do` | pass | p50 | cost |
|---|---|---|---|
| agent mode (default) | **35/40** | 9.8 s | $0.15 |
| program, compiler sees the page | 16/40 | 17.9 s | $0.24 |
| program from the request only; logic-free programs run as the request | 17/40 | 18.5 s | $0.21 |
| + Jev gate: program only when the request states logic (removed) | 34/40 | 10.9 s | $0.11 |

Logic-free → request: 10/10. Programs with logic: 7/30; the 5 gated tasks that do
state a condition ("coupon only if over 200", "if the name is taken use …"): agent
mode 5/5, programs 0/5 (invented addresses, click-level steps, `if true`). Agent
mode already handles in-request conditions within one goal. Not yet measured: long
mechanical loops (page until X, first of 30 that matches), where the IR should
help; held-out-3 has none.

## 2026-09-26 · Scraping: English → program → streamed JSONL (`bench/scrape`)

`fab do` routes a request to collect data from a list (one Jev question, keyword
fallback) to a program: Claude Sonnet 5 compiles it (`FAB_COMPILE_MODEL`; pilot:
Sonnet 36/36 structurally right vs gpt-6-luna 35, luna-pro 34, kimi-k2.6 32,
haiku-4.5 30), then the VM runs it. Scraping ops run in code (`js/scrape.js`):
items = sibling groups by signature (multi-row records, ads with other classes
excluded), fields = structural paths an LLM maps once per layout from coverage-
chosen samples, leaf detail pages fetched in parallel and parsed in the page,
client-rendered details opened for real, `next page` covers rel=next, Next/›,
page numbers, Load more and endless scroll, dedup across batches.

Eval: 18 generated sites, 1,684 gold records, exact field match. Held-out-5 first
sight: 2/5 exact (then used for tuning, so no longer blind). Final, three runs in a
row: **18/18 exact, 1,684/1,684 records, every field**, 192-214 s for the suite
(browser start + compile per case); 1,000 products with 1,000 detail pages in 18 s.
Agent-mode suite (held-out-3) through the new `fab do` routing: 37/40 (agent mode
alone: 35/40).

## 2026-09-26 · Page pool: concurrent requests to one session run in parallel tabs

Before: a session daemon ran one command at a time (a mutex around its one
`Session`); concurrent `fab -s X do …` and MCP calls queued, and when they were
different callers they also overwrote each other's page. One browser per profile
is a hard limit (Firefox profile lock, Chrome SingletonLock; Firefox allows one
WebDriver session), so the unit of concurrency is a tab: `crates/fab/src/pool.rs`
keeps up to `FAB_POOL_MAX` (default 4) pages per browser, each a `Session` of its
own on its own tab (`fab_core::Pages`: CDP targets / BiDi browsing contexts in the
same browser, cookies and logins shared). The first tab is "home" and never closes.

- **Lease + affinity.** A request leases one page for its whole run. Requests name
  a client (`FAB_CLIENT` for the CLI, one per MCP server process); a client's next
  request returns to the page its last one finished on. Choice: own idle page
  (skips the line) → a never-used page → a new page while under max → the least
  recently used page no longer held → wait in a FIFO line (`FAB_POOL_WAIT`, 300 s;
  more than `FAB_POOL_QUEUE`=64 waiting fail at once). A client moved to another
  page (its own busy or reclaimed) starts at the address it was last on.
- **Scale down.** Pages idle `FAB_POOL_IDLE` (60 s) close down to `FAB_POOL_MIN` (1).
- **Reliability.** Before each lease the page must answer a script in 3 s, else it
  moves to a fresh tab (hung/crashed renderer) or is replaced; a tab closed from
  outside is followed/recreated by the page's own view, and the main view never
  moves onto a pool page's tab. A lease dropped without release (request over
  `FAB_POOL_LEASE`=900 s, panic, cancellation) discards its page (home: kept and
  re-checked). In `--connect` mode every tab fab opens is closed at the end, and
  only those.

Stress harness `bench/pool/stress.py` (now `fab-bench pool`): 12 clients at once, each two dependent
steps on a fixture form (type its own username; then, with no URL, submit — the
submitted address must carry its username), headless, `FAB_MODEL=none`; then the
same one client at a time; then idle scale-down. `max=1 hold=0` is the old
behaviour (one page, FIFO).

| config | browser | parallel (12 × 2 steps) | correct | sequential | peak / after idle |
|---|---|---|---|---|---|
| one page (pre-pool) | Firefox | 11.8 s | **1/12** | 13.1 s | 1 / 1 |
| one page (pre-pool) | Chrome | 12.2 s | **1/12** | 12.7 s | 1 / 1 |
| pool max 6, no hold | Firefox | 7.0 s | 3/12 | 46.7 s | 6 / 1 |
| pool max 6, fixed 10 s hold | Firefox | 22.2, 24.4 s | 12/12 | 55.1 s | 6 / 1 |
| + adaptive hold | Firefox | 14.5, 9.3 s | 12/12 | 45.8 s | 6 / 1 |
| + no background-tab timer clamp | Firefox | **5.6, 5.2 s** | 12/12 | 12.6 s | 6 / 1 (4.6 s) |
| same | Chrome | **5.6, 5.1 s** | 12/12 | 12.0 s | 6 / 1 (5.7 s) |

- **Hold.** Without one, a newcomer took an idle page between a client's two steps
  and the typed form was lost (address-forking can't carry page state): 3/12.
  When the pool is full a page is now kept for its client for 3× that client's
  usual gap between requests (1 s to `FAB_POOL_HOLD`=10 s; 10 s before a client's
  second request). A fixed 10 s hold was correct but made waiters sit out every
  finished client's hold.
- **Firefox background tabs.** Pool pages are background tabs, where Firefox
  clamps `setTimeout` to 1 s and settle polls with timers: step-2 median 1.24 s →
  0.30 s with `dom.min_background_timeout_value`=4 and budget throttling off (the
  equivalent of Chrome's `--disable-background-timer-throttling`).
- A race let a pool reach max+1 (the opening counter dropped before the new page
  was counted): fixed, and a 60-task unit test checks max is never exceeded.
- MCP: `tools/call` requests now run concurrently (`bench/pool/mcp.py`, since removed with the MCP server; 6 calls at
  once, max 4): all 6 replies correct in 0.73–1.67 s (shared session) and
  0.73–1.55 s (`--own`), vs ~4.5 s one after another.
- Live checks (`pool::live`, ignored by default), both browsers: a hung page (`for(;;)`)
  is renewed on a fresh tab in 3.0 s; Chrome `Page.crash` and a tab closed through
  CDP recover; home untouched. `--connect` to a running Chrome: 4 concurrent clients
  in 4 tabs, 3 reaped when idle, and after `fab close` only the user's own tab remained.
- Not done: scraping client-rendered details in parallel tabs. Programs open one
  item at a time (`open item` … `back`), so it needs speculative prefetch in the
  scraper; only `csr_details` (18.8 s) would gain.

## 2026-09-26 · Hacker News replies: general root causes; `threads_top3` flake measured

Goal: `fab do "every post on the front page: the top 3 upvoted replies with author
and body" --url https://news.ycombinator.com/` streams exact `{author, body}`
records. Independent check (`fab-bench scrape verify-hn`): each story's first
three top-level comments fetched and parsed separately. Runs: 82/82, 83/83, 80/83
(2 ranks moved between scrape and check, 1 comment edited by its author), 82/87
(5 ranks moved, 0 wrong). Fixes, all general:

- `open` picked the external link: the mapper now sees the request and what the
  program reads after opening.
- The compiler invented a vote sort; "top N" is the first N in site order. Null
  numeric comparisons are false instead of an error.
- Threaded lists are flat rows: items carry a `level` (indent attribute, visual
  indent or spacer width) and the mapper can keep `only_level`.
- Parallel fetches hit 503s cached as empty pages: fetch retries with backoff under
  per-host adaptive concurrency; `goto` retries too.
- Rich text starting with `<i>` was truncated: one rich-text leaf with lines kept.
- Presentational digit classes (`c00` vs `c5a`) broke paths: class matching
  tolerates only classes containing digits (tolerating any class broke invoices).
- The pager clicked in-page `#` anchors and aria-hidden links; `count >= 3 or not
  next page` paged anyway (eager effects): conditions short-circuit.
- An item with no mapped link opened another link; it now opens an empty page.
- The mapper sometimes answered two JSON objects and "Wait…": the last one wins.
- Only paths ending in `@href` count as links; the "thin page" test compares the
  rendered text in a hidden iframe; seen items are scoped per list and document.

`threads_top3` (throttled route, 24 stories, 48 records) is still flaky. With the
Rust harness (`fab-bench scrape run threads_top3 --repeat N`): 3/5 exact before any
change. Every miss is the first opened story (`item-900`, 3 records): its
comment list gets "none on this page" because the list mapper answers
`"list": null` for identical candidates it maps correctly on the next story (the
learn-from-the-richest-fetched-page change does not help; the candidates were
already the right ones). It happens when the compiled program calls the items
"replies" rather than "top-level comments". Asking the mapper once more on a null
answer gave 6/8 exact; not a measured win at this sample size, so it was not
kept. Open: make the list choice for an opened page not depend on one sampled
answer (for example, decide once per layout from several items' pages).

Full suite with the current engine (event-stream output): 19/20 exact, 1,743/1,746
records (`threads_top3` 45/48).

## 2026-09-26 · Bench tooling in Rust (`fab-bench scrape …`, `fab-bench pool`)

The scrape eval and pool stress moved from Python/shell into `fab-bench`:
`scrape gen` (generator; a CPython-compatible Mersenne Twister makes its output
byte-identical to the old `gen.py`: `diff -r` over 1,503 files is empty, so gold
and fixtures are unchanged), `scrape run [--arm words|schema] [--repeat N]`,
`scrape score`, `scrape pilot`, `scrape verify-hn`, and `pool`. They drive the
`fab` CLI and read its JSONL events. Cross-checks: the Rust scorer gives the same
score as `score.py` on the same stream (threads_top3 45/48, R 0.9375); `pool`
gives 12/12 on Chrome (6.0 s parallel vs 12.4 s sequential) and Firefox (5.4 s
vs 12.6 s), scaling back to one page. Generated fixtures are no longer
committed (`fixtures/scrape/` is ignored); run `fab-bench scrape gen`.

## 2026-09-26 · Words vs declared records (`--arm schema`); checkpoint growth

Full suite, one run each: words 19/20 exact (1,743/1,746 records), schema 17/20
(1,678/1,746). The three differing cases, 3 runs per arm: words 6/9 exact,
schema 7/9. The words arm's misses include a `threads_top3` run whose 48
records used other field names (0 matched); declared fields rule that out. The
schema arm's misses: the compiler wrote an unsupported `items "…" from section`
under the declared fields (now reported as `invalid_program`, open: the compile
spec), and a rich-text body read as lines, which a string field rejected; text
lines now coerce to one string joined by newlines, as the gold has it. Equal
accuracy at this sample size; the structured request stays the agent default.

Journaled `fab run` of the 1,000-product `bigcat` scrape: the checkpoint grew to
~340 KB by the end and was rewritten on every one of ~5,100 requests (58 s, vs
19 s for the same scrape as one unjournaled call). Items the program no longer
holds now drop their fields and links (they are only read through a held
handle), and at most 30 fetched pages per layout are kept (only 8 and 30 are
read): ~195 KB at the end, 52 s, output still exact. Open: forgotten items still
keep their list selector and key (~130 KB at 850 items), and `seen` grows with
every item.

## 2026-09-27 · No regular expressions in the in-page runtimes; declarative field steps

`snapshot.js` and `scrape.js` hold no regex any more (none was left in Rust). The
field mapper's `{"path", "re"}` became declarative steps that `scrape.js` applies
in order: `after` / `before` (text after / before a marker's first occurrence),
`part: {sep, index}` (0-based, -1 last), `take: "number"` (first number with its
`,`/`.` separators), `not` (none when the text contains it) and `has` (the word
found, for yes/no fields). Evidence for the set: every `"re"` in past logs was a
number (`([\d.,]+)`, `Serves (\d+)`, `([\d.]+) out of 5`), a flag
(`(Sponsored)`, `^(?!No comments yet\.)`), or a prefix/suffix cut (`Prep: (.+)`,
`^(.*?)\s*\(\d+\)$`, `· ([A-Za-z]+)$`). The other ~50 regex uses were rewritten as
plain string code; a differential fuzz against each original regex (7.75 M
checks, and \s over the whole BMP) found 0 mismatches.

Full suite, 2 runs each: before 19/20, 18/20 (threads_top3 45/48; csr_details
in-stock nulls). After, first prompt: 18/20, 15/20; the mapper took "169 m²" as
a number (the prompt now says `take` is for counts, scores and ratings only, and
a measure keeps its unit), plus a news_sections compile flake and the known
csr_details nulls. After, final prompt: 20/20, 20/20 (1,746/1,746 records).

## 2026-09-28 · Reddit front page: top comment of the top 10 posts

Request: `{"do": "for each of the first 10 posts on the front page, open its
comments and get the top-level comment shown first (skip pinned moderator or bot
comments)", "url": "https://www.reddit.com/", "records": {…rank, title,
subreddit, comment_author, comment}}`. old.reddit.com now redirects logged-out
visitors to a login, and Reddit's JSON API blocks scripts, so this runs on the
web-component site. Fixes, all general:

- Item pages whose list is built in the browser (comments loaded by script) are
  opened for real when their fetched copy lacks the list, for the first such
  page and for later ones whose list is already known; the fetched copy stays in
  use when the real page doesn't have it either.
- A live list's first read waits until it stops growing, and list choice on a
  live page waits until the candidate lists stop changing.
- Of candidate lists of the same items (a thread and the replies nested in one
  comment), the fullest is kept.
- An item's fields exclude nested items of its own kind (a comment's replies).
- Custom elements' attributes are sampled leaves (`.@author`,
  `@post-title`); a bare `@attr` resolves to the item's own or the nearest one.
- A path with sibling positions resolves at other positions when exactly one
  element matches; a rich block's first paragraph (`…>p`) reads as the block.
- `back` makes the item's list the one `next page` pages again; a pager control
  that loads nothing (or a link elsewhere) falls back to scrolling the list.
- Items with no mapped field and no link among linked items (ads) are skipped.
- Records carry the live page URL; a compile that doesn't parse gets a second
  retry.

Result: 10/10 posts with comments; an independent agent-mode read of each
post's first top-level comment agreed on 8/10 (the other two readings were
inconsistent: author of one comment, text of another). Two of three consecutive
runs gave identical records; the third kept one moderator comment (the
mapper's flag for moderator/bot comments varies). Scrape suite unchanged:
20/20 exact, 1,746/1,746 records.

## 2026-09-29 · `threads_top3`: the list-choice prompt is not what decides it (rejected)

`threads_top3` (`fab-bench scrape run threads_top3 --repeat 10 --headless`, the
Rust harness, anthropic/claude-sonnet-5.5) is **8/10 exact** on the current
engine: 8 runs 48/48, one run 0/48, one run 45/48 plus a renderer crash. The
two failures are not the list choice at all:

- one run mapped the front page's item link to the external article
  (`0|td.title>span.titleline>a@href`) instead of the comments link
  (`1|td.subtext>span.subline>a:2@href`), so all 24 detail fetches failed with
  `TypeError: NetworkError`, every item opened an `example.org` page with no
  comments, and the run emitted 0 records in ~130 s. This is the one-sampled-
  answer class again, but on the **link**, not the list: the mapper's answer is
  never checked against the page it leads to.
- one run lost the browser ("session closed") or found the Firefox profile
  already in use ("profile … is open in a Firefox fab can't control"), and
  scored 0/48 in 1 s without running the program.

The documented list flake (the first opened story answered `"list": null` for
candidate lists the same mapper picks on the next story) is **rare**: it did not
occur once in the 20 runs below, so it cannot be shown to be fixed at this
sample size.

**Rejected: telling the mapper that the program's word for the items is not the
page's word.** The `ITEM_SYSTEM` bullet about picking the list became "judge by
what the items ARE, the values you can read for the wanted fields, not by how
close the word is", and a list choice on an item's own page gained the line
"This page is an item's own page … the wanted items are a list on it unless it
really has none". Measured, 10 + 10 repeats: **8/10 and 7/10 exact (15/20)**,
against the **8/10** baseline. Not a win, so it was reverted: the prompt is not
what decides this case, and biasing the mapper away from a null answer risks
inventing a list on a page that really has none (a story without comments).

Also not worth retrying without a way to force the failure: the answer-side
redundancies (`ask` a second time on a null list, re-ask on the real page — the
latter already happens in `choose`) all resample the same evidence, and the
evidence-side variants (learn the list from the richest fetched page of the
layout, wait until the candidate lists stop changing) are already in the engine
and did not help when they were added.

## 2026-09-29 · The ~60 s first-step stall: what the code says, and what it costs

Never reproduced in this session (7+10 fresh isolated headless first steps on
local fixtures: 12.5-13.6 s each, no gap), so nothing here is claimed as the
cause. What the search did find is worth recording, because two of the shapes
are exactly 60 s and one of them was costing real time on every call.

Measured, not speculative: a request refused for good (HTTP 402, no credits)
was retried **5 times with 11.0-11.8 s of backoff** before the caller heard the
answer that had already been decided (`llm.rs` ORed "the body has an `error`
key" into "retryable", so 401/402/400 were retryable too). Only a rate limit, a
server fault, or a success body without a message is worth sending again.
Measured on the same run: the bench harness was reporting `paused` and
`0/48` records against a key that could never answer.

Code shapes that fit a ~60 s first-step-only stall, in order:

- `jev.rs`: 30 s client timeout, three transport attempts with **no backoff**
  between them, so a hung connect costs 30 s, then 30 s again. Only the first
  call pays it (the connection is pooled afterwards), which is the shape of a
  first-step-only stall. Now backs off like its other retries.
- `secrets/bw.rs`: `WAIT = 60 s` for a locked `bw`; `op` and the helper wait
  90 s. The stores are listed concurrently, so the slowest one sets the wall
  clock and the step is silent for a minute. `bw` is not installed on this
  machine, so this is a shape, not a diagnosis; the timeout now says what to do.
- `backend/cdp.rs`: the `--connect` version probe had **no timeout at all**
  (a blackholed host hangs on the OS TCP timeout, minutes). Now 5 s.
- `backend/cdp.rs`: `Conn::open` gives the connect path 60 s, and two missed
  CDP calls are 30 s each, on a fresh browser where a miss is likeliest.
- `agent.rs`: `Session::new` joins the browser launch with the decider
  warm-up, so a slow first decider call holds the whole first step.

Cheapest experiment that separates the first two: point the decider at a
blackhole (`TYPESAFE_BASE_URL` to an unroutable host) and compare a first step
against the same step with `FAB_SECRETS=none`, both on a machine where `bw` is
installed and locked. The first shows 30 s/30 s retry warnings; only the second
names a password manager in its output.

Not reproducible here also means: the fix list above is bounded waits and
honest messages, not a root cause. The stall may still be the network.

## 2026-09-29 · Proving the link before following it (rejected)

The damage measured above is not the list choice: it is the mapper's **link**
answer, which is also one sample and was never checked against the page it
leads to. The change measured here treats the link as a hypothesis: after the
parallel fetch of a list's items' pages, **when not one of them could be read**,
the mapper is asked once more for a different link (a note saying the old one
leads nowhere readable), the items' links are read again in code with it, and
the new link is taken only when a page it leads to does load. Bounded to one
retry per list (`link_retried`), no navigation and no page mutation, and the
old behaviour is untouched whenever the proof fails.

Measured (release binary, same harness):

| | threads_top3 (10 repeats) | full suite |
|---|---|---|
| before (engine as of c1c87c2) | **8/10** exact (8× 48/48, one 0/48 wrong link, one 0/48 renderer crash) | 20/20, 1,746/1,746 |
| with the link proof | **6/10** exact | 19/20 and 17/20, 1,746/1,746 then **1,682/1,746** |

So it was reverted. Two things are worth recording beyond the verdict:

- **The proof never fired once.** Its note ("the link to follow led nowhere
  readable") appears in no run log, not even in the runs that mapped the link
  to the external article and fetched `0/24` detail pages. The re-ask, given
  the same leaves and a note, answered the same wrong link; asked twice, the
  mapper is as consistent as it is wrong. Re-asking with a note is therefore
  **not** a way out of a wrong answer, whichever field the answer is about.
- **What that failure looks like**: `fetched 0/24 detail pages`, every item
  then opened for real on an `example.org` page with no comments, 0 records in
  ~90-140 s. A fix has to *not trust* the answer and read the page anyway (e.g.
  accept a link only when the page it opens holds the list the program then
  asks for, and try the next candidate link otherwise), not ask again.
- The batch was also much noisier than the morning's: 3 of the 10
  `threads_top3` runs lost the browser outright ("session closed"), and the two
  full-suite runs each had a different `directory_all`/`forum_comments` miss on
  a field path (`website` null, F 0.861/0.893) with 1,746/1,746 and
  1,682/1,746 records. Those field misses are mapper sampling, unrelated to
  the link, and they are the noise floor of this eval right now.

## 2026-09-29 · Trying the items' other links when the mapped link reads nowhere (kept)

Not a re-ask (measured above: the mapper returns the same wrong link) but a
walk over what the page already shows. When a list's link to follow was mapped
and **none** of the detail pages could be fetched (`ok == 0`, more than one
URL), the program reads the `@href` paths on the sampled items (`sampleLeaves`),
drops the failed one, orders the rest **own-site first** (same origin as the
page; a link to another site is usually an external article or an author),
takes at most 3 (`LINK_TRIES`) and, for each, reads the items' links in code
and fetches up to 3 of the pages (`LINK_PROBES`). The first path whose pages
include one that reads becomes the list's link, the items' links are replaced,
the dead pages leave `fetched` and all the new pages are fetched in parallel.
If no path proves out, nothing changes. Once per list (`link_retried`); no
model call, no navigation; general (no selectors, no regex). The pure ordering
and bound are unit-tested (`link_candidates_own_site_first_and_bounded`).

Measured (release binary, same harness):

| | threads_top3 (10 repeats) | full suite |
|---|---|---|
| before | **8/10** exact | 20/20, 1,746/1,746 |
| with own-site-first link fallback | **10/10** exact (10× 48/48) | **20/20, 1,746/1,746** |

The fallback fired in 2 of the 10 runs (`fetched 0/24 detail pages`, then
`the link to follow led nowhere readable: 1|td.subtext>span.subline>a:2@href
opens a page that does`), each of which finished 48/48 — the case that was
0/48 before. `cargo test --workspace --locked --offline` is green.

## 2026-09-29 · `fab-bench warm` (port of `bench/warm.sh`)

`fab-bench warm [--suite S] [--tag T] [--planner-model M,..] [--paraphrases N] [--jobs J]`:
per fixture, the first task in suite order learns (`shape=record`); every later task runs twice
at once, `shape=use` and `shape=off`, then the paired `summary`. Each arm is a `fab-bench bench`
child logging to `<out>/<tag>-{learn,use,cold}.log`. Port unverified against the script: the
script drives `fab bench`, which `fab` no longer has, so it can't run; `bench/warm.sh` is kept.
Smoke run (router, 1 task pair, 1 paraphrase): learned 1 site; cold fail, use pass (43.7s vs 89.7s).

## 2026-09-29 · Sonnet 5.5 as the compile model: two general fixes (kept)

Switching the compile default to `anthropic/claude-sonnet-5.5` dropped the
scrape suite to 19/20. Two causes, both fixed without anything
site-specific:

- **Nested lists on the outer item's own page.** 5.5 listed a section's
  articles while still on the index (`news_sections` 0/6). The compile spec
  now says `items` only sees the page the browser is on, so an inner list
  on the outer item's page needs `open` first and `back` after its loop.
  `news_sections` 7/8 exact (the miss: a byline mapped with its date).
- **Mapper paths that were never shown.** The page mapper sometimes named a
  `label:…` path that none of its sample pages has (`recipes_all`: 1/8
  exact, `prep_time` null on every record). The recovery only re-mapped when
  *every* field read null. Now each mapped path is run through the same
  resolver against the sample pages, and fields whose path is on none of
  them are asked once more, with the paths named. `recipes_all` 8/8 (the
  re-ask fired in 6 of the 8 runs).

Full suite after both: 18/20 then 20/20 (1746/1746). The two misses in the
first run are unrelated flakes: the mapper kept "By " on an author, and one
compile returned an empty program (fell back to one step).
