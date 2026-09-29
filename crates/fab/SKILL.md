---
name: fab
description: Hand web work to fab (fast-agentic-browser) - one step at a time - and get structured results back. fab opens pages, navigates, fills forms, signs in with the user's saved logins, reads tables, scrapes lists and answers. Use when a task needs a website or a local web app - looking something up, collecting data from a list, filling a form, changing a setting, checking a dev server.
allowed-tools: Bash(fab:*)
---

# fab

Say what you want done, and declare what you want back. fab does the clicking,
typing, reading and signing in itself, far faster than driving a browser step
by step, and streams the result as JSON Lines.

## Ask with a JSON request

Declare the shape of the results up front; fab never guesses what to emit.

```bash
fab do '{"do": "every job on all pages, with the salary from its own page",
         "url": "jobs.example.com",
         "records": {"type": "object",
                     "properties": {"title": {"type": "string"},
                                    "company": {"type": "string"},
                                    "salary": {"type": ["number", "null"], "description": "yearly, in the posted currency"},
                                    "link": {"type": "string", "format": "uri"}},
                     "required": ["title", "company"]}}'

fab do '{"do": "what is the total of refunded orders in March?", "url": "localhost:3000/orders",
         "returns": {"type": "number"}}'
```

- `do`: the goal in words. `url`: open it first (optional).
- `records`: JSON Schema of each streamed record. Flat scalar fields only: `type` is `string`, `number`, `integer` or `boolean` (or `[that, "null"]`), plus `format: "uri"`, `enum` and `description`. Nested objects, arrays, `$ref` and `oneOf` are rejected with the JSON pointer. Every record has every field, in your order (`null` when a field is not required and missing). Values are coerced: `"$1,299"` → `1299`, relative links → absolute, rich text read as lines → one string with `\n` between lines.
- `returns`: JSON Schema of the single answer value (`end.value`).
- A value that doesn't fit ends the stream with `schema_mismatch`, never a reshaped record.
- `fab --schema` prints the request and event schemas. Pass a long request on stdin: `fab do - < request.json`.

For a plain question or action, words are fine: `fab do "log in to github.com"`, `fab do "star the ianks/fast-agentic-browser repo"`. The answer comes back as a string.

## Read the stream

stdout is JSON Lines, one event per line; `-v` adds progress on stderr.

```jsonl
{"t":"start","v":1,"ts":"…","cmd":"do","task":"5e03…","schema":{…}}
{"t":"record","seq":0,"ts":"…","url":"https://jobs.example.com/j/1","data":{"title":"…","company":"…","salary":95000,"link":"…"}}
{"t":"end","ts":"…","url":"…","ok":true,"value":null,"records":55,"error":null}
```

- `record.data` is the object you declared. `seq` numbers records from 0; `url` is the page each came from.
- The answer is `end.value`. On failure `end.error.code` says why: `invalid_args`, `invalid_program`, `navigation_failed`, `step_failed`, `schema_mismatch`, `secret_unavailable`, …
- Exit status: 0 done, 1 failed, 2 rejected before anything ran, 3 paused because an action may have happened: `fab tasks show TASK`, then `fab tasks resolve …` and `fab tasks resume TASK`. Never just rerun a paused step.
- `jq -c 'select(.t=="record").data'` keeps just the records. `--request-id ID` makes a retry safe: the same id returns the same records and answer instead of doing the work again.

## Working with fab

- **One step per call.** Name the goal, not the clicks: "cancel order 1042", not "click the second button". A step can be big; break a long job into steps when you need an answer in between.
- **Steps share a session:** the same browser, page and logins. `-s NAME` runs separate sessions in parallel; steps sent to one session at the same time run side by side in their own tabs (set `FAB_CLIENT` per worker so each one's next step continues on its own tab). `fab close` ends a session.
- **Keep the user's exact values in double quotes** inside `do`: `"rename the project to \"Q3 Launch\""`.
- **Secrets and personal details:** never type or make them up. Sign-ins happen by themselves when a step says "log in" or lands on a sign-in form. Anything else from the user's password manager goes in as `{{plain words}}`, e.g. `"pay with {{card number}}, expiry {{expiry}}, CVC {{CVC}}"`. Values show as `••••`. If fab says an item isn't approved, show the user the `fab secrets allow …` command it prints.
- **Programs:** `fab do '{…}' --print` returns the program fab would run in `end.value`; edit it and `fab run FILE --records '<schema>'`. See `fab --help` for the grammar.
- Destructive or paying steps (delete, pay, send) only when the user asked for them.
