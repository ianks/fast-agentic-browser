# fab's output

Every command prints [JSON Lines](https://jsonlines.org) on stdout, one event
per line. The exceptions are documentation, printed as text and exiting 0:
`--help`, `--version`, `--skill` (the agent skill) and `--schema` (the JSON
Schemas of requests and events). Argument errors end the stream like any other
failure (`invalid_args`, exit 2), with the usage on stderr; a crash still
prints an `internal` `end`. Progress goes to stderr, and only with `-v`. The events are defined
once, in [`crates/fab/src/events.rs`](../crates/fab/src/events.rs).

```jsonl
{"t":"start","v":1,"ts":"2026-09-26T16:23:25.467Z","cmd":"run","task":"5e030e79577ddd070cb1fadfc76a4017"}
{"t":"record","seq":0,"ts":"2026-09-26T16:23:25.600Z","url":"https://shop.test/p/1","data":{"name":"Kettle","price":42}}
{"t":"record","seq":1,"ts":"2026-09-26T16:23:25.601Z","url":"https://shop.test/p/2","data":{"name":"Mug","price":9}}
{"t":"end","ts":"2026-09-26T16:23:25.603Z","url":"https://shop.test/","ok":true,"value":2,"records":2,"error":null}
```

| `t` | when | fields |
| --- | --- | --- |
| `start` | exactly once, first | `v` (contract version, 1), `ts`, `cmd`, `task` (when the command runs as a durable task), `schema` (the records' shape, when known) |
| `record` | 0..n, in emit order | `seq`, `ts`, `url`, `data` |
| `item` | 0..n (listings: `tasks list`, `sessions`, `doctor`, `secrets`) | `data` |
| `end` | exactly once, last, on success and failure | `ts`, `url`, `ok`, `value`, `records`, `error` |

## Reading a stream

Read lines until `t` is `end`, then check `ok`. If no `end` arrives, the process
died and the outcome is unknown: the `task` from `start` is how to find out
(`fab -s SESSION tasks show TASK`, `fab -s SESSION tasks output TASK`; tasks
belong to the session that ran them).

```ruby
IO.popen(["fab", "do", request]).each_line do |line|
  ev = JSON.parse(line)
  case ev["t"]
  when "record" then rows << ev["data"]
  when "end"    then raise ev["error"]["message"] unless ev["ok"]
  end
end
```

- `record.data` is always an object keyed by name, in emit order: `emit price`
  gives `{"price": 42}`, `emit a: x, b: y` gives `{"a": …, "b": …}`. There is one
  shape to parse. fab never merges, groups or nests records; a nested scrape
  emits one flat record per leaf, repeating parent fields the program includes.
- `seq` counts a task's records from 0 without gaps. `(task, seq)` names a
  record. A resumed task continues its numbering, so a gap before the first
  `seq` means earlier records are in `fab tasks output TASK`.
- `url` on a record is the page it was read from (an item's own page for
  fields read there); null for a computed record. `url` on `end` is where the
  browser was left.
- `ts` is RFC 3339 UTC with milliseconds. Replayed records keep their original
  `ts`; an `end` carries the time its stream ended.
- `end.value` is the program's `return` value (an array for several) or the
  step's answer; `do --print` puts the program text there.
- `end.records` is how many records the task has committed in all.

## Declaring the output

Agents send a request instead of words, with the records' shape as JSON
Schema (flat scalar fields: `string`, `number`, `integer`, `boolean`, each
optionally nullable, plus `format: "uri"`, `enum`, `description`):

```bash
fab do '{"do": "every job on all pages", "url": "jobs.example.com",
         "records": {"type": "object", "properties": {"title": {"type": "string"}, "salary": {"type": ["number", "null"]}},
                     "required": ["title"]}}'
```

Every record then has exactly those fields, in that order, coerced to their
types (`"$1,299"` → 1299, relative links → absolute); a value that does not fit
ends the stream with `schema_mismatch`. `returns` declares `end.value` the same
way. `start.schema` repeats the declared schema; for a request in words it
shows the fields the program names (untyped). `fab --schema` prints the JSON
Schemas of the request and of every event.

## Errors

`end.error` is null on success, otherwise:

```json
{"code": "step_failed", "message": "no luck", "line": 2, "uncertain": false}
```

| `code` | meaning | exit |
| --- | --- | --- |
| `invalid_program`, `invalid_args` | rejected before any browser effect | 2 |
| `navigation_failed`, `step_failed`, `budget_exhausted`, `secret_unavailable`, `schema_mismatch`, `cancelled`, `internal` | failed after starting | 1 |
| `paused`, `interrupted` | may have acted: resolve with `fab tasks resolve`, then `fab tasks resume` | 3 |

`uncertain` is true when browser effects may have happened before the failure.
`line` is the program line, for failures inside a program.

Consequences worth knowing:
- A `do` in words that fails after it may have acted ends `paused` (exit 3),
  not `step_failed`: whether the page changed is unknown until you check.
- Numbers must state one number: `"$1,299.50"` and `"721 points"` are numbers;
  `"call 555-1234"`, `"2024-05-01"`, `"1.2k"` and `true` are not
  (`schema_mismatch`). Whole numbers print as integers.
- A `format: "uri"` field must hold a link; blank text is missing, and text
  like `"N/A"` is a `schema_mismatch`.
- A field that isn't `required` is emitted as `null` when missing, and
  `start.schema` marks it nullable. `enum` text matches ignoring case and
  surrounding space, and the declared spelling is emitted. Text read as
  several lines is joined with `\n`.

## Delivery

Records are committed to the task before they are printed. A command that
repeats a `--request-id` reprints the same records and answer instead of doing
the work again; `fab tasks output TASK [--after SEQ]` reprints them at any time.
Delivery is at least once: deduplicate by `(task, seq)`.

The contract version `v` changes only when the meaning of an existing field
changes. New fields may appear; ignore the ones you don't know.
