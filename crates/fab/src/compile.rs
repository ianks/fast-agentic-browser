//! A sentence → a line script (`fab_core::script`), by a cheap LLM from a
//! short spec. The spec is the one that measured best with ten cheap models
//! on held-out requests (83% of programs fully correct, vs 22% for typed JSON
//! steps); its wording matters: a terser rule line gets copied into scripts.

use anyhow::{Result, bail, ensure};
use fab_core::llm::ChatUsage;
use fab_core::script;
use serde_json::json;

pub const SPEC: &str = r#"Write the user's request as a program for a browser robot, with only the logic known in advance. Three examples:
do "book a cut with Inès at Maison Verlaine on Saturday at 11:30 for Camille Fontaine, mobile {{my mobile number}}"

set n = 1
while n <= 30
  do "open the comments of story $n on news.ycombinator.com"
  if test "the page shows a comment"
    read "the author of the first comment" -> author
    return author
  end
  set n = n + 1
end
fail "no story has comments"

set pages = 1
while true
  for job in items "job listings"
    extract "title, company, remote" from job -> row
    if row.remote != null
      open job
      extract "salary" -> detail
      back
      emit title: row.title, company: row.company, salary: detail.salary
    end
  end
  if pages >= 3 or not next page
    break
  end
  set pages = pages + 1
end
Steps: do "<sub-goal>"; read "<question>" -> x; test "<yes/no question>".
Scraping, done in code (fast and exact): items "<what the items are>" gives the items on the page not seen yet; extract "<field, field>" from <item> -> x reads an item's fields, and without "from" the current page's; open <item> ["<which link>"] follows the item's link (say which when an item links to several pages, e.g. "comments" rather than the article it points to) and back returns; next page shows the next page, batch, or month of a calendar and is false at the end; emit key: value, … streams one record, keyed with the user's words. Fields are named like row.price; a field the item or page lacks is null, and a yes/no field (row.sponsored, row.on_sale) is null when no. In a threaded list (comments, replies) item.level is 0 for top-level items and higher for replies to them: the replies to a post or story are its top-level comments (level 0); only replies to a comment are deeper. Compare with ==, >, and contains(text, "part"); a field with several values (labels) is a list, and contains(row.labels, "bug") tests it.
Logic: set, if/else/end, while/end, for x in <list>/end, break, stop, fail "<why>", return <values>.
Most requests are a single do. Add an if or a loop only when the request repeats, searches, collects, or depends on something only known while running. A do is a whole sub-goal: the robot works out the pages, fields and clicks itself, so never spell them out, but name a site by its web address (news.ycombinator.com). When the user wants data from a list, write a scraping program: you can't see how many pages or batches it has, so unless the user limits the pages, always loop with next page (it is false when there are no more; it also loads more on "Load more" and endless-scroll pages). A limit on items ("the first 20", "5 per section") counts the records you emit and still loops with next page, since a page may hold fewer. Emit one record per item with the fields the user named, using the user's words for field names (extract "section name" …, then refer to it as sec.section_name), in the user's order, opening an item only for fields that are on its own page; to filter, extract the field the condition needs; in nested loops, take an outer item's fields from the outer item (extract "name" from category -> cat). items only sees the page the browser is on: when the inner list is on the outer item's own page (the articles of a section, the products of a category), open the outer item before listing it and back after its loop. Get fields only with extract (it is code: fast, exact, and null when the page lacks the field), never with read or test. Lists come in the site's own order, and sites rank their lists themselves without showing the votes or scores behind the ranking: for "the top N", "the most upvoted N" or "the best N", take the first N in order; never extract a vote or score field to sort by unless the user says the page shows it. Emit exactly the fields the user asks to get for each record ("…: replies with author and body" → author and body): no title or other field of an outer item unless the user asked to get it. {{plain words}} stands for the user's secrets and personal details, never invented; values the user gave are written as given. Output only the program."#;

/// The spec's two example programs.
pub fn example() -> String {
    SPEC.lines().skip(1).take_while(|l| !l.starts_with("Steps:")).collect::<Vec<_>>().join("\n")
}

/// The program grammar, for people and agents that write programs themselves.
/// (Not in the compile prompt: cheap models learn from the examples; a
/// grammar alone measured far worse.)
pub const GRAMMAR: &str = r#"steps  do "<sub-goal>"             fab works out the pages and clicks; yields what it reports
       read "<question>" -> x       a value from the site
       test "<yes/no question>"     a check of the page (-> x, or inline: if test "…")
logic  set x = <expr>   if <cond> … else … end   while <cond> … end   for x in <list> … end
       break   fail "<why>"   return <values>
       break   stop
values "text with $x"  12.5  x  row.price  null  [a, b]  a + 1  x > 200 and not done  contains(a, "b")
scrape items "<what>"   extract "<fields>" [from item] -> x   open item ["<which link>"]   back
       next page (false at the end)   emit x, y (streams one JSON record)
       {{plain words}}: a secret from the password manager"#;

/// The model that writes programs: `FAB_COMPILE_MODEL`, else the default.
pub fn model() -> String {
    std::env::var("FAB_COMPILE_MODEL").ok().filter(|m| !m.is_empty()).unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5.5";

/// Compiles `sentence` into a script that parses. Errors when the model fails
/// or writes something that doesn't parse; callers fall back to the sentence
/// as a one-line script.
pub async fn compile(model: &str, sentence: &str, here: Option<&str>, shape: &Shape) -> Result<(String, ChatUsage)> {
    let llm = crate::planner::llm_for(model)?;
    // Only the address the browser is on: shown the page itself, models plan
    // clicks and branch on guesses about the UI.
    let mut user = match here {
        Some(u) => format!("{sentence}\n\n(The browser is already open on {u}.)"),
        None => sentence.to_string(),
    };
    // The caller's declared output is a contract, not a hint.
    if let Some(r) = &shape.records {
        user.push_str(&format!("\n\n{}", r.instruction()));
    }
    if let Some(f) = &shape.returns {
        user.push_str(&format!("\n\nEnd with `return` of one value: {}.", f.kind_text()));
    }
    // Declared fields replace guessing which fields the user meant.
    let problems = |p: &script::Program| -> Vec<String> {
        match &shape.records {
            Some(r) => shape_problems(p, r),
            None => {
                let extra = unasked_keys(p, sentence);
                if extra.is_empty() { vec![] } else { vec![format!("the records have fields the user didn't ask for ({}); emit only the fields the user named", extra.join(", "))] }
            }
        }
    };
    let extras = |p: &script::Program| -> Vec<String> {
        match &shape.records {
            Some(r) => emitted_keys(p).into_iter().filter(|k| !r.names().contains(k)).collect(),
            None => unasked_keys(p, sentence),
        }
    };
    let mut body = json!({
        "messages": [{"role": "system", "content": SPEC}, {"role": "user", "content": user}],
        "temperature": 0,
        "max_tokens": 1500,
        // Measured with reasoning off; some providers refuse to turn it off.
        "reasoning": {"enabled": false},
    });
    let (msg, usage, _) = match llm.chat(body.clone()).await {
        Ok(r) => r,
        Err(e) if format!("{e:#}").to_lowercase().contains("reasoning") => {
            body.as_object_mut().unwrap().remove("reasoning");
            llm.chat(body.clone()).await?
        }
        Err(e) => return Err(e),
    };
    let text = strip_fences(msg["content"].as_str().unwrap_or(""));
    // One retry, told what doesn't parse or looks wrong.
    let problem = match script::parse(&text) {
        Err(e) => format!("That doesn't parse: {e}. Write the whole program again, using only the statements and functions shown."),
        Ok(p) => {
            let mut l = script::lint(&p);
            l.extend(problems(&p));
            if l.is_empty() {
                return Ok((text, usage));
            }
            format!("Check this: {}. Write the whole program again.", l.join("; "))
        }
    };
    body["messages"].as_array_mut().unwrap().extend([json!({"role": "assistant", "content": text}), json!({"role": "user", "content": problem})]);
    let (msg2, usage2, _) = llm.chat(body.clone()).await?;
    let mut text2 = strip_fences(msg2["content"].as_str().unwrap_or(""));
    let mut usage = ChatUsage { prompt_tokens: usage.prompt_tokens + usage2.prompt_tokens, completion_tokens: usage.completion_tokens + usage2.completion_tokens, cost: usage.cost + usage2.cost };
    // Neither parses: once more, told what's wrong with the retry.
    if let (Err(e), Err(_)) = (script::parse(&text2), script::parse(&text)) {
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role": "assistant", "content": text2}),
            json!({"role": "user", "content": format!("That doesn't parse either: {e}. Use only the statements and functions shown in the grammar; write the whole program again.")}),
        ]);
        let (msg3, usage3, _) = llm.chat(body).await?;
        text2 = strip_fences(msg3["content"].as_str().unwrap_or(""));
        usage = ChatUsage { prompt_tokens: usage.prompt_tokens + usage3.prompt_tokens, completion_tokens: usage.completion_tokens + usage3.completion_tokens, cost: usage.cost + usage3.cost };
    }
    match script::parse(&text2) {
        // Fields still added that the user didn't ask for: dropped.
        Ok(p) => {
            let fixed = drop_keys(&text2, &extras(&p));
            // What's left must still satisfy the declared records.
            if let Some(r) = &shape.records {
                let missing = shape_problems(&script::parse(&fixed)?, r);
                ensure!(missing.is_empty(), crate::events::Failure::new(crate::events::ErrorCode::InvalidProgram, format!("the program cannot produce the declared records: {}\n{fixed}", missing.join("; "))));
            }
            Ok((fixed, usage))
        }
        // The retry broke it: the first program, when it parsed.
        Err(e) if script::parse(&text).is_err() => bail!("the compiled script doesn't parse ({e}):\n{text2}"),
        Err(_) => Ok((text, usage)),
    }
}

/// The output a caller declared (a structured request).
#[derive(Debug, Clone, Default)]
pub struct Shape {
    pub records: Option<crate::request::RecordSchema>,
    pub returns: Option<crate::request::Field>,
}

/// Keys the `emit`s name, in order. A bare variable is left out: it may
/// spread a record whose fields are only known at run time.
fn emitted_keys(p: &script::Program) -> Vec<String> {
    use script::{Expr, Op};
    let mut out = vec![];
    for o in p.ops() {
        let Op::Emit(es) = o else { continue };
        for (key, e) in es {
            if let Some(k) = match (key, e) {
                (Some(k), _) => Some(k.clone()),
                (None, Expr::Var(v)) if v.contains('.') => Some(v.rsplit('.').next().unwrap_or(v).to_string()),
                _ => None,
            } {
                if !out.contains(&k) {
                    out.push(k);
                }
            }
        }
    }
    out
}

/// How a program's `emit`s differ from the declared records: fields not
/// declared, required fields never written, or no `emit` at all.
fn shape_problems(p: &script::Program, r: &crate::request::RecordSchema) -> Vec<String> {
    use script::{Expr, Op};
    let emits: Vec<&Vec<(Option<String>, Expr)>> = p.ops().iter().filter_map(|o| if let Op::Emit(es) = o { Some(es) } else { None }).collect();
    if emits.is_empty() {
        return vec!["it never emits a record; emit one per result with the declared fields".into()];
    }
    let names = r.names();
    let mut out = vec![];
    let extra: Vec<String> = emitted_keys(p).into_iter().filter(|k| !names.contains(k)).collect();
    if !extra.is_empty() {
        out.push(format!("emit only the declared fields; not {}", extra.join(", ")));
    }
    for es in emits {
        // A bare record variable spreads fields only known at run time.
        if es.iter().any(|(k, e)| k.is_none() && matches!(e, Expr::Var(v) if !v.contains('.'))) {
            continue;
        }
        let written: Vec<String> = es.iter().filter_map(|(k, e)| k.clone().or_else(|| if let Expr::Var(v) = e { v.rsplit('.').next().map(str::to_string) } else { None })).collect();
        let missing: Vec<&str> = r.fields.iter().filter(|f| f.required && !written.contains(&f.name)).map(|f| f.name.as_str()).collect();
        if !missing.is_empty() {
            out.push(format!("an emit is missing the required field{} {}", if missing.len() == 1 { "" } else { "s" }, missing.join(", ")));
            break;
        }
    }
    out
}

/// Removes `key: value` parts with these keys from `emit` lines (a record
/// keeps at least one field).
fn drop_keys(src: &str, keys: &[String]) -> String {
    if keys.is_empty() {
        return src.to_string();
    }
    src.lines()
        .map(|l| {
            let t = l.trim_start();
            let Some(rest) = t.strip_prefix("emit ") else { return l.to_string() };
            let parts: Vec<&str> = rest.split(", ").collect();
            let keep: Vec<&str> = parts
                .iter()
                .copied()
                .filter(|p| {
                    let k = p.split_once(':').map(|(k, _)| script::field_name(k.trim().trim_matches('"'))).or_else(|| p.rsplit_once('.').map(|(_, f)| f.trim().to_string()));
                    !k.is_some_and(|k| keys.contains(&k))
                })
                .collect();
            if keep.is_empty() {
                return l.to_string();
            }
            format!("{}emit {}", &l[..l.len() - t.len()], keep.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keys of emitted records that no word of the request names ("title" when
/// the user asked for "author and body").
fn unasked_keys(p: &script::Program, request: &str) -> Vec<String> {
    use script::{Expr, Op};
    let split = |t: &str| t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2).map(str::to_string).collect::<Vec<_>>();
    let words = split(request);
    // A word the request uses only to name a list ("every post …") names
    // the items, not a field: count it against the programs' list names.
    let lists: Vec<String> = p
        .ops()
        .iter()
        .flat_map(|o| match o {
            Op::ForInit { list: e, .. } | Op::Set { expr: e, .. } => e.effects(),
            _ => vec![],
        })
        .filter(|(k, _)| *k == script::Eff::Items)
        .flat_map(|(_, d)| split(&d))
        .collect();
    let same = |a: &str, b: &str| a.starts_with(&b[..b.len().min(5)]) || b.starts_with(&a[..a.len().min(5)]);
    let named = |k: &str| {
        k.split('_').filter(|w| w.len() > 2).any(|w| {
            let in_req = words.iter().filter(|r| same(r, w)).count();
            let as_list = lists.iter().filter(|r| same(r, w)).count().min(1);
            in_req > as_list
        })
    };
    let mut out = vec![];
    for o in p.ops() {
        let Op::Emit(es) = o else { continue };
        for (key, e) in es {
            let k = match (key, e) {
                (Some(k), _) => k.clone(),
                (None, Expr::Var(v)) if v.contains('.') => v.rsplit('.').next().unwrap_or(v).to_string(),
                _ => continue,
            };
            if !named(&k) && !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// The script inside a ``` block, when the model wrote one.
fn strip_fences(t: &str) -> String {
    let t = t.trim();
    if let Some(start) = t.find("```") {
        let body = &t[start + 3..];
        let body = body.split_once('\n').map(|(_, b)| b).unwrap_or("");
        let body = body.find("```").map(|e| &body[..e]).unwrap_or(body);
        return body.trim().to_string();
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unasked_fields() {
        let p = script::parse("for x in items \"posts\"\n  extract \"title, author\" from x -> r\n  emit title: r.title, author: r.author\nend").unwrap();
        assert_eq!(unasked_keys(&p, "every post: the replies with author and body"), ["title"]);
        let q = script::parse("for x in items \"posts on the front page\"\n  extract \"title\" from x -> r\n  emit post: r.title\nend").unwrap();
        assert_eq!(unasked_keys(&q, "every post on the front page: replies with author and body"), ["post"]);
        let d = script::parse("for c in items \"categories in the directory\"\n  extract \"name\" from c -> r\n  emit category: r.name\nend").unwrap();
        assert!(unasked_keys(&d, "For every category in the directory, give me category, name and phone").is_empty());
        assert!(unasked_keys(&p, "every post's title and author").is_empty());
        let p = script::parse("for x in items \"jobs\"\n  extract \"employment type\" from x -> r\n  emit r.employment_type\nend").unwrap();
        assert!(unasked_keys(&p, "jobs with their employment types").is_empty());
    }

    #[test]
    fn drops_keys() {
        let src = "for x in items \"a\"\n  emit story: s.title, author: r.author, body: r.body\nend";
        assert_eq!(drop_keys(src, &["story".into()]), "for x in items \"a\"\n  emit author: r.author, body: r.body\nend");
    }

    #[test]
    fn fences() {
        assert_eq!(strip_fences("Here you go:\n```text\nopen a.io\nclick Go\n```\nDone."), "open a.io\nclick Go");
        assert_eq!(strip_fences("open a.io\nclick Go"), "open a.io\nclick Go");
    }

    #[test]
    fn spec_example_parses() {
        let p = script::parse(&example()).unwrap();
        assert!(p.ops().len() > 5);
    }

    #[test]
    fn declared_records_constrain_the_program() {
        let r = crate::request::RecordSchema::parse(&json!({"properties": {"title": {"type": "string"}, "votes": {"type": "integer"}}, "required": ["title"]})).unwrap();
        let problems = |src: &str| shape_problems(&script::parse(src).unwrap(), &r);
        assert!(problems("for s in items \"stories\"\n  emit title: s.title, votes: s.points\nend").is_empty());
        assert!(problems("for s in items \"stories\"\n  emit s.title\nend").is_empty(), "votes may be missing");
        assert!(problems("for s in items \"stories\"\n  emit s\nend").is_empty(), "a spread record is checked at run time");
        assert!(problems("for s in items \"stories\"\n  emit votes: s.points\nend")[0].contains("required field title"));
        assert!(problems("for s in items \"stories\"\n  emit title: s.title, url: s.link\nend")[0].contains("not url"));
        assert!(problems("return 1")[0].contains("never emits"));
        assert_eq!(drop_keys("  emit title: s.title, url: s.link", &["url".into()]), "  emit title: s.title");
    }
}
