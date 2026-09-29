# fast-agentic-browser (`fab`)

Ever noticed how painfully sluggish it is for agents to use a web browser?
`fab`'s sole mission is to fix that. A beacon of perf in the hellscape of clunky
agentic browser tooling. It's also cheaper and more accurate than the
alternatives.

![The same task, the same cheap LLM. Left: chrome-devtools-mcp. Right: fab.](https://raw.githubusercontent.com/ianks/fast-agentic-browser/main/docs/race.gif)

<sub>"Customize my October training block": two session swaps among look-alikes,
a new court day, save. Same LLM (mercury-2.5), real time. Left: the LLM driving
chrome-devtools-mcp, 35.0 s. Right: fab, 2.7 s.</sub>

Your agent says what it wants done, in words. fab does the clicking, typing,
reading and signing in, and hands back the result.

|                      | LLM + chrome-devtools-mcp | fab       |
| -------------------- | ------------------------- | --------- |
| tasks passed         | 73.1%                     | **92.5%** |
| median time per task | 40.7 s                    | **16.6 s** |
| cost per task        | $0.0133                   | **$0.0044** |

<sub>A blind suite of 40 tasks on 20 unseen apps, same planner LLM, strict
outcome checks, measured 2026-09-24. How it's measured: [`bench/LOG.md`](https://github.com/ianks/fast-agentic-browser/blob/main/bench/LOG.md)
("Blind gate: held-out-3").</sub>

## Use it

```bash
fab do "open news.ycombinator.com and tell me the top story and its points"
fab do "search for Ada Lovelace and tell me when she was born" --url en.wikipedia.org
fab do "log in to github.com"                                  # your saved login, 2FA included
fab do "what's the total of refunded orders?" --url localhost:3000/orders    # exact, across every page
fab do "pay with {{card number}}, expiry {{expiry}}, CVC {{CVC}}, but don't place the order"
fab do 'sign up as "qa+1@acme.dev" with {{new password}}' --url localhost:3000/signup
fab do '{"do": "every job on all pages", "url": "jobs.example.com",
         "records": {"properties": {"title": {"type": "string"}, "salary": {"type": ["number", "null"]}}}}'
fab close
```

Agents declare what they want back as JSON Schema and get exactly that shape
([docs/output.md](https://github.com/ianks/fast-agentic-browser/blob/main/docs/output.md)); words are for people.

### Is this an MCP and/or <harness> extension?

No, and there never will be. For the first time in history, the dreams of the
Unix philosophy are finally being realized by LLMs. One-shotting bash pipelines
is viable now. This is an **extremely** powerful way to shape data, and MCP prevents 
that. Just let the LLMs use bash, sheesh.

**One output format.** Every command prints JSON Lines: a `start` event, a
`record` per result, and one `end` with the answer or a typed error, on success
and failure alike ([docs/output.md](https://github.com/ianks/fast-agentic-browser/blob/main/docs/output.md)).

**Scrape in plain English.** Ask for data from a list and fab streams a record
per item as it pages through it, opening each item's page when a field lives
there:

```bash
fab do "every job on all pages: title, company, and the salary from its page" --url jobs.example.com \
  | jq -c 'select(.t=="record").data' > jobs.jsonl
```

fab writes the request's logic as a small program (loops, conditions, limits;
`--print` shows it, `fab run` runs one). The items, fields, pages and detail
pages are handled by code in the page: an LLM maps each page layout to its fields
once, detail pages are fetched in parallel, and values are copied exactly. The
eval (`bench/scrape`, 20 cases on generated sites, 1,746 records: numbered
pages, Next buttons, Load more, endless scroll, two-row records, 3-level
nesting, client-rendered details, sponsored items, missing fields) is exact on
all 20 in most runs, with an occasional miss where the LLM maps a field
wrong; 1,000 products with their pages take about 20 s.

**Secrets never reach your agent.**
- Logins come from 1Password, Bitwarden or the macOS Keychain, and are typed
  only on the site they're saved for.
- `{{plain words}}` stands for anything else in your password manager.
  Passwords, codes, card numbers and keys stay `••••` everywhere, and fab asks
  you once per site before it uses a card, an identity or a key.
- A `{{new password}}` is generated and saved before it's typed.
- `fab secrets` shows what's connected.

## Install

```bash
cargo install --locked fast-agentic-browser
fab doctor
```

High-level steps need an `OPENROUTER_API_KEY` (in the environment or
`~/.config/fab/env`).

**Agents** drive fab through the CLI:
- **The CLI skill:** `fab --skill` prints it, `fab --skill --install` adds it to
  Claude Code, and `--install codex|agents|<dir>` puts it elsewhere. Commands
  on one session (`-s NAME`, default `default`) share one browser window and
  its logins.
- **Concurrency:** requests that overlap (several `fab do` at once) run side
  by side in tabs of the same browser, sharing its logins; each caller's next
  step continues on its own tab (name callers with `FAB_CLIENT`). At most
  `FAB_POOL_MAX` tabs (default 4) — more requests wait their turn — and idle
  tabs close after `FAB_POOL_IDLE` seconds (60).
- **Durable tasks:** a request survives a crash. `fab tasks list` shows them,
  `fab tasks resume ID` continues one from its last confirmed step, and
  `fab tasks resolve` settles a step whose outcome is uncertain (did the
  click land?) before anything is repeated.

**Browser.** fab uses your default browser (Chrome, Edge, Brave, Firefox, …) in
a window you can watch, with its own profile. `--headless` hides the window,
`--browser firefox` picks one, and `--connect auto` works in your running Chrome.

## Development

`fab-bench` runs the suites, races and the decision dataset
(`fab-bench bench --mode agent --suite bench/heldout3.toml`), and the scrape
eval (`fab-bench scrape run`). `scripts/demo` opens a live browser with a
terminal wired to it, for trying requests by hand. The history of
every change and its measured effect is in [`bench/LOG.md`](https://github.com/ianks/fast-agentic-browser/blob/main/bench/LOG.md).

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/ianks/fast-agentic-browser/blob/main/LICENSE-APACHE) or
[MIT license](https://github.com/ianks/fast-agentic-browser/blob/main/LICENSE-MIT) at your option.
