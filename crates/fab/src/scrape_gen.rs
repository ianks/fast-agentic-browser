//! Generates the scraping eval: fixture sites (fixtures/scrape/) and, per
//! case, the exact records a correct scrape streams (bench/scrape/gold/*.jsonl).
//!
//! A byte-exact port of the original `bench/scrape/gen.py`: same seed, same
//! sequence of `random.Random` calls (via [`crate::pyrand`]), same HTML, and
//! JSON formatted the way Python's `json.dumps` formats it.
//!
//! Sites mix the layouts and traps real scrapes hit: numbered pagination,
//! Next buttons without URL changes, Load more, infinite scroll, two-row
//! records, 3-level nesting, client-rendered detail views, interleaved ads and
//! sponsored items, missing fields, label/value details, decoy lists (nav,
//! footer, related items).

use crate::pyrand::Random;
use anyhow::Result;
use std::path::{Path, PathBuf};

const SEED: u64 = 20260926;

const FIRST: &str = "Ada Grace Linus Margaret Alan Barbara Ken Dennis Frances Edsger Radia Tim Hedy Katherine Donald John Sophie Anita Leslie Jean Niklaus Guido Yukihiro Rasmus Bjarne Brendan Larry Rich James Joan";
const LAST: &str = "Lovelace Hopper Torvalds Hamilton Turing Liskov Thompson Ritchie Allen Dijkstra Perlman Berners-Lee Lamarr Johnson Knuth McCarthy Wilson Borg Lamport Sammet Wirth Rossum Matsumoto Lerdorf Stroustrup Eich Wall Hickey Gosling Clarke";
const CITIES: [&str; 8] = ["Oslo", "Bergen", "Berlin", "Lisbon", "Remote", "Stockholm", "Amsterdam", "Copenhagen"];

/// Regenerates both trees (removing whatever was there) and returns each gold
/// file's name and record count, sorted by name.
pub fn generate(fixtures: &Path, gold: &Path) -> Result<Vec<(String, usize)>> {
    let _ = std::fs::remove_dir_all(fixtures);
    let _ = std::fs::remove_dir_all(gold);
    std::fs::create_dir_all(fixtures)?;
    std::fs::create_dir_all(gold)?;
    let mut g = Gen {
        r: Random::new(SEED),
        fix: fixtures.to_path_buf(),
        gold: gold.to_path_buf(),
        first: FIRST.split_whitespace().collect(),
        last: LAST.split_whitespace().collect(),
        counts: Vec::new(),
    };
    g.jobs()?;
    g.shop()?;
    g.forum()?;
    g.directory()?;
    g.spa()?;
    g.feed()?;
    g.invoices()?;
    g.recipes()?;
    g.issues()?;
    g.news()?;
    g.deals()?;
    g.events()?;
    g.bigcat()?;
    g.threads()?;
    g.csr()?;
    let mut counts = g.counts;
    counts.sort();
    Ok(counts)
}

// ------------------------------------------------------------ Python semantics

/// `html.escape(s)` (quote=True).
fn e(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Inserts `,` thousands separators into a run of digits (with optional `-`).
fn group_digits(digits: &str) -> String {
    let (sign, d) = digits.strip_prefix('-').map_or(("", digits), |d| ("-", d));
    let mut out = String::new();
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// `f"{n:,}"` for an int.
fn comma(n: i64) -> String {
    group_digits(&n.to_string())
}

/// `f"{v:,.2f}"`. Rust's `{:.2}` rounds the exact binary value half-to-even,
/// as Python does.
fn comma2(v: f64) -> String {
    let s = format!("{v:.2}");
    let (int, frac) = s.split_once('.').unwrap();
    format!("{}.{frac}", group_digits(int))
}

/// The sites' price format: `f"${v:,.2f}"`.
fn money(v: f64) -> String {
    format!("${}", comma2(v))
}

/// `round(x, 2)`: CPython rounds via the correctly-rounded decimal string.
fn round2(x: f64) -> f64 {
    format!("{x:.2}").parse().unwrap()
}

/// `repr(float)` for the modest magnitudes used here.
fn float_repr(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e16 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

/// `f"{x:g}"` for the one-decimal ratings used here.
fn float_g(x: f64) -> String {
    format!("{x}")
}

/// A JSON value with Python's dict ordering.
#[derive(Clone)]
enum J {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
}

fn s(x: impl Into<String>) -> J {
    J::Str(x.into())
}

fn opt(x: &Option<String>) -> J {
    x.as_ref().map_or(J::Null, |v| J::Str(v.clone()))
}

/// `json.dumps(v, ensure_ascii=...)` with the default separators.
fn dumps(v: &J, ensure_ascii: bool) -> String {
    let mut out = String::new();
    dump(v, ensure_ascii, &mut out);
    out
}

fn dump(v: &J, ascii: bool, out: &mut String) {
    match v {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Int(n) => out.push_str(&n.to_string()),
        J::Float(f) => out.push_str(&float_repr(*f)),
        J::Str(t) => dump_str(t, ascii, out),
        J::Arr(xs) => {
            out.push('[');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump(x, ascii, out);
            }
            out.push(']');
        }
        J::Obj(kv) => {
            out.push('{');
            for (i, (k, x)) in kv.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump_str(k, ascii, out);
                out.push_str(": ");
                dump(x, ascii, out);
            }
            out.push('}');
        }
    }
}

fn dump_str(t: &str, ascii: bool, out: &mut String) {
    out.push('"');
    for c in t.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ------------------------------------------------------------ shared markup

fn page(title: &str, body: &str, script: &str) -> String {
    let title = e(title);
    format!(
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>{title}</title>
<meta name="viewport" content="width=device-width">
<style>body{{font-family:system-ui;margin:0}} header,footer{{background:#eee;padding:8px}} main{{padding:12px}}
.card{{border:1px solid #ddd;margin:6px;padding:8px}} .pager a,.pager span{{margin:0 4px}} .disabled{{color:#aaa}}</style>
</head><body>
<header><nav><ul class="menu"><li><a href="/">Home</a></li><li><a href="#">About</a></li><li><a href="#">Careers</a></li><li><a href="#">Contact</a></li><li><a href="#">Help</a></li></ul></nav></header>
<main>{body}</main>
<footer><ul class="links"><li><a href="#">Privacy</a></li><li><a href="#">Terms</a></li><li><a href="#">Cookies</a></li><li><a href="#">Accessibility</a></li></ul></footer>
{script}</body></html>"##
    )
}

fn pager(cur: usize, n: usize, href: impl Fn(usize) -> String) -> String {
    let mut parts = Vec::new();
    parts.push(if cur > 1 {
        format!(r#"<a href="{}" rel="prev">« Prev</a>"#, href(cur - 1))
    } else {
        r#"<span class="disabled">« Prev</span>"#.to_string()
    });
    for i in 1..=n {
        if n > 7 && 2 < i && i + 1 < n && i.abs_diff(cur) > 1 {
            if i == 3 || i + 2 == n {
                parts.push("<span>…</span>".to_string());
            }
            continue;
        }
        parts.push(if i == cur {
            format!(r#"<span aria-current="page" class="current">{i}</span>"#)
        } else {
            format!(r#"<a href="{}">{i}</a>"#, href(i))
        });
    }
    parts.push(if cur < n {
        format!(r#"<a href="{}" rel="next">Next »</a>"#, href(cur + 1))
    } else {
        r#"<span class="disabled">Next »</span>"#.to_string()
    });
    format!(r#"<nav class="pager" aria-label="Pagination">{}</nav>"#, parts.concat())
}

/// `lst[(p-1)*per : p*per]`.
fn chunk<T>(xs: &[T], p: usize, per: usize) -> &[T] {
    let lo = ((p - 1) * per).min(xs.len());
    let hi = (p * per).min(xs.len());
    &xs[lo..hi]
}

struct Gen {
    r: Random,
    fix: PathBuf,
    gold: PathBuf,
    first: Vec<&'static str>,
    last: Vec<&'static str>,
    counts: Vec<(String, usize)>,
}

impl Gen {
    fn write(&self, path: &str, text: &str) -> Result<()> {
        let p = self.fix.join(path);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, text)?;
        Ok(())
    }

    fn gold(&mut self, case: &str, recs: Vec<J>) -> Result<()> {
        let mut out = String::new();
        for r in &recs {
            out.push_str(&dumps(r, false));
            out.push('\n');
        }
        let file = format!("{case}.jsonl");
        std::fs::write(self.gold.join(&file), out)?;
        self.counts.push((file, recs.len()));
        Ok(())
    }

    fn pick(&mut self, xs: &[&str]) -> String {
        self.r.choice(xs).to_string()
    }

    fn first(&mut self) -> String {
        let xs = self.first.clone();
        self.pick(&xs)
    }

    fn last(&mut self) -> String {
        let xs = self.last.clone();
        self.pick(&xs)
    }

    fn name(&mut self) -> String {
        let f = self.first();
        let l = self.last();
        format!("{f} {l}")
    }

    // ------------------------------------------------------------ 1. jobs
    fn jobs(&mut self) -> Result<()> {
        struct Job {
            id: i64,
            title: String,
            company: String,
            location: String,
            salary: Option<String>,
            typ: String,
            days: i64,
        }
        let titles = ["Backend Engineer", "Frontend Developer", "Data Scientist", "SRE", "Product Designer", "Staff Engineer", "ML Engineer", "Engineering Manager", "QA Engineer", "Security Engineer", "iOS Developer", "Rust Engineer"];
        let cos = ["Northwind", "Contoso", "Fabrikam", "Initech", "Globex", "Umbrella Labs", "Hooli", "Vandelay", "Stark Industries", "Wayne Tech"];
        let mut jobs = Vec::new();
        for i in 0..55 {
            let title = self.pick(&titles);
            let company = self.pick(&cos);
            let location = self.pick(&CITIES);
            let nok = format!("NOK {}", comma(self.r.randint(55, 140) * 10000)).replace(',', " ");
            let lo = self.r.randint(50, 130);
            let hi = self.r.randint(131, 180);
            let eur = format!("€{lo},000 – €{hi},000");
            let salary = self.r.choice(&[None, Some(nok), Some(eur)]);
            let typ = self.pick(&["Full-time", "Contract", "Part-time"]);
            let days = self.r.randint(1, 40);
            jobs.push(Job { id: 1000 + i * 7, title, company, location, salary, typ, days });
        }
        let (per, n) = (12, 5);
        let mut rec: Vec<&Job> = Vec::new();
        for p in 1..=n {
            let part = chunk(&jobs, p, per);
            let mut cards = Vec::new();
            for (k, j) in part.iter().enumerate() {
                if k == 4 {
                    cards.push(r#"<article class="card promo"><h3><a href="/ads/1">Hiring? Post a job for free</a></h3><p>Reach 2M engineers</p></article>"#.to_string());
                }
                cards.push(format!(
                    r#"<article class="card job"><h3 class="title"><a href="job-{}.html">{}</a></h3>
<p class="meta"><span class="company">{}</span> · <span class="loc">{}</span></p>
<p class="age">Posted {} days ago</p></article>"#,
                    j.id, e(&j.title), e(&j.company), e(&j.location), j.days
                ));
            }
            let side = r##"<aside><h4>Popular searches</h4><ul class="tags"><li><a href="#">Rust jobs</a></li><li><a href="#">Remote jobs</a></li><li><a href="#">Oslo jobs</a></li><li><a href="#">Senior jobs</a></li></ul></aside>"##;
            let body = format!(
                r#"<h1>Open positions</h1>{side}<section class="results">{}</section>{}"#,
                cards.concat(),
                pager(p, n, |q| format!("page-{q}.html"))
            );
            self.write(&format!("jobs/page-{p}.html"), &page(&format!("Jobs – page {p}"), &body, ""))?;
            for j in part {
                let mut dl = String::new();
                if let Some(sal) = &j.salary {
                    dl += &format!("<dt>Salary</dt><dd>{}</dd>", e(sal));
                }
                dl += &format!("<dt>Employment type</dt><dd>{}</dd><dt>Location</dt><dd>{}</dd>", j.typ, j.location);
                let body = format!(
                    r#"<p><a href="page-{p}.html">← Back to results</a></p><h1>{t}</h1><p class="co">{c}</p>
<dl class="facts">{dl}</dl><section><h2>About the role</h2><p>{t} at {c}. You will build things.</p></section>
<section><h2>Similar jobs</h2><ul><li><a href="job-{}.html">{}</a></li><li><a href="job-{}.html">{}</a></li><li><a href="job-{}.html">{}</a></li></ul></section>"#,
                    jobs[0].id,
                    e(&jobs[0].title),
                    jobs[1].id,
                    e(&jobs[1].title),
                    jobs[2].id,
                    e(&jobs[2].title),
                    t = e(&j.title),
                    c = e(&j.company),
                );
                self.write(&format!("jobs/job-{}.html", j.id), &page(&j.title, &body, ""))?;
                rec.push(j);
            }
        }
        let details = rec
            .iter()
            .map(|j| J::Obj(vec![("title", s(&j.title)), ("company", s(&j.company)), ("location", s(&j.location)), ("salary", opt(&j.salary)), ("employment_type", s(&j.typ))]))
            .collect();
        let list = rec.iter().map(|j| J::Obj(vec![("title", s(&j.title)), ("company", s(&j.company)), ("location", s(&j.location))])).collect();
        let first2 = rec[..24].iter().map(|j| J::Obj(vec![("title", s(&j.title)), ("company", s(&j.company))])).collect();
        let oslo = rec
            .iter()
            .filter(|j| j.location == "Oslo")
            .map(|j| J::Obj(vec![("title", s(&j.title)), ("company", s(&j.company)), ("salary", opt(&j.salary))]))
            .collect();
        self.gold("jobs_details", details)?;
        self.gold("jobs_list", list)?;
        self.gold("jobs_first2", first2)?;
        self.gold("jobs_oslo", oslo)
    }

    // ------------------------------------------------------------ 2. shop (load more, sponsored, split price)
    fn shop(&mut self) -> Result<()> {
        struct Prod {
            id: String,
            name: String,
            price: f64,
            was: Option<f64>,
            rating: f64,
            reviews: i64,
            sponsored: bool,
            stock: String,
            sku: String,
        }
        let nouns = ["Headphones", "Keyboard", "Monitor", "Webcam", "Mouse", "Dock", "Speaker", "Microphone", "Tablet", "Charger", "SSD", "Router"];
        let adj = ["Pro", "Air", "Max", "Mini", "Ultra", "Lite", "Studio", "Go", "Plus", "X"];
        let brands = ["Aurora", "Nimbus", "Vertex", "Quasar", "Helix", "Orbit"];
        let mut prods = Vec::new();
        for i in 0..46 {
            let base = self.r.choice(&[19.0, 29.0, 49.0, 79.0, 99.0, 149.0, 199.0, 249.0, 399.0, 1299.0]);
            let price = base + self.r.choice(&[0.0, 0.5, 0.99]);
            let sale = self.r.random() < 0.3;
            let id = format!("P{}", 3000 + i * 13);
            let b = self.pick(&brands);
            let nn = self.pick(&nouns);
            let a = self.pick(&adj);
            let name = format!("{b} {nn} {a}");
            let was = sale.then(|| round2(price * 1.25));
            let rating = self.r.choice(&[3.5, 4.0, 4.2, 4.5, 4.8, 5.0]);
            let reviews = self.r.randint(0, 2500);
            let stock = self.pick(&["In stock", "In stock", "Only 3 left", "Out of stock"]);
            let sku = format!("SKU-{}", self.r.randint(100000, 999999));
            prods.push(Prod { id, name, price, was, rating, reviews, sponsored: i % 9 == 4, stock, sku });
        }
        let mut data = Vec::new();
        for p in &prods {
            data.push(J::Obj(vec![
                ("id", s(&p.id)),
                ("name", s(&p.name)),
                ("price", s(money(p.price))),
                ("was", p.was.map_or(J::Null, |w| s(money(w)))),
                ("rating", J::Float(p.rating)),
                ("reviews", J::Int(p.reviews)),
                ("sponsored", J::Bool(p.sponsored)),
            ]));
            let brand = p.name.split_whitespace().next().unwrap();
            let specs: String = [("SKU", p.sku.as_str()), ("Brand", brand), ("Warranty", "2 years"), ("Availability", p.stock.as_str())]
                .iter()
                .map(|(k, v)| format!("<tr><th>{k}</th><td>{v}</td></tr>"))
                .collect();
            let also: String = prods[..4]
                .iter()
                .map(|q| format!(r#"<div class="card"><a href="{}.html">{}</a> <span class="price">{}</span></div>"#, q.id, e(&q.name), money(q.price)))
                .collect();
            let body = format!(
                r#"<nav class="crumbs"><a href="../index.html">Shop</a> › {n}</nav><h1>{n}</h1>
<div class="buy"><span class="price">{}</span> <button>Add to cart</button></div>
<table class="specs"><tbody>{specs}</tbody></table>
<h2>Customers also bought</h2><div class="also">{also}</div>"#,
                money(p.price),
                n = e(&p.name)
            );
            self.write(&format!("shop/p/{}.html", p.id), &page(&p.name, &body, ""))?;
        }
        let script = r#"<script>
const DATA = %s;
let shown = 0;
function price(p){ const [d,c] = p.price.slice(1).split('.'); return `<span class="now"><span class="cur">$</span>${d}<sup>.${c}</sup></span>` + (p.was ? ` <s class="was">${p.was}</s>` : ''); }
function more(){
  const g = document.querySelector('#grid');
  for (const p of DATA.slice(shown, shown + 12)) {
    const d = document.createElement('div'); d.className = 'card product';
    d.innerHTML = (p.sponsored ? '<span class="badge">Sponsored</span>' : '') +
      `<a class="name" href="p/${p.id}.html">${p.name}</a><div class="price">${price(p)}</div>` +
      `<div class="rating" aria-label="${p.rating} out of 5 stars">★ ${p.rating}</div><span class="reviews">(${p.reviews.toLocaleString('en-US')})</span>`;
    g.appendChild(d);
  }
  shown += 12;
  if (shown >= DATA.length) document.querySelector('#more').remove();
}
setTimeout(more, 150);
document.querySelector('#more').onclick = () => { const b = document.querySelector('#more'); b.disabled = true; b.textContent = 'Loading…'; setTimeout(() => { more(); const m = document.querySelector('#more'); if (m) { m.disabled = false; m.textContent = 'Load more'; } }, 400); };
</script>"#
            .replacen("%s", &dumps(&J::Arr(data), true), 1);
        self.write("shop/index.html", &page("Shop", r#"<h1>All products</h1><div id="grid" class="grid"></div><button id="more">Load more</button>"#, &script))?;
        let details = prods
            .iter()
            .filter(|p| !p.sponsored)
            .map(|p| J::Obj(vec![("name", s(&p.name)), ("price", s(money(p.price))), ("rating", s(float_g(p.rating))), ("sku", s(&p.sku)), ("availability", s(&p.stock))]))
            .collect();
        let sale = prods
            .iter()
            .filter(|p| p.was.is_some_and(|w| w != 0.0) && !p.sponsored)
            .map(|p| J::Obj(vec![("name", s(&p.name)), ("price", s(money(p.price))), ("old_price", s(money(p.was.unwrap())))]))
            .collect();
        self.gold("shop_details", details)?;
        self.gold("shop_sale", sale)
    }

    // ------------------------------------------------------------ 3. forum (HN-like two-row records)
    fn forum(&mut self) -> Result<()> {
        struct Comment {
            author: String,
            text: String,
        }
        struct Story {
            id: i64,
            title: String,
            points: i64,
            author: String,
            site: Option<String>,
            nc: i64,
            comments: Vec<Comment>,
        }
        let words: Vec<&str> = "Show HN: A tiny Rust VM for scraping;Ask HN: How do you paginate APIs?;The unreasonable effectiveness of tables;Why SQLite is everywhere;A tour of WebAssembly GC;Postgres 19 released;Building a browser engine in a weekend;The case for boring tech;Lisp in 100 lines;How we cut our AWS bill by 80%;Understanding CRDTs;A visual guide to TCP;Zig 1.0;Notes on distributed tracing;The history of Unix pipes".split(';').collect();
        let mut stories = Vec::new();
        for i in 0..75i64 {
            let many = self.r.randint(1, 300);
            let nc = self.r.choice(&[0, 0, many]);
            let mut comments = Vec::new();
            for k in 0..nc.min(3) {
                let who = self.first().to_lowercase();
                let author = format!("{who}{}", self.r.randint(1, 99));
                comments.push(Comment { author, text: format!("Comment {k} on story {i}.") });
            }
            let title = self.pick(&words) + &if i < 15 { String::new() } else { format!(" ({})", 2000 + i) };
            let points = self.r.randint(1, 900);
            let who = self.last().to_lowercase();
            let author = format!("{who}_{}", self.r.randint(1, 9));
            let site = self.r.choice(&[Some("github.com"), Some("blog.example.org"), Some("nytimes.com"), None]).map(str::to_string);
            stories.push(Story { id: 48000000 + i * 31, title, points, author, site, nc, comments });
        }
        let mut rec = Vec::new();
        for p in 1..=3usize {
            let mut rows = Vec::new();
            for (k, st) in chunk(&stories, p, 25).iter().enumerate() {
                let rank = (p - 1) * 25 + k + 1;
                let site = st.site.as_ref().map_or(String::new(), |x| format!(r#" <span class="sitebit">(<span class="sitestr">{x}</span>)</span>"#));
                let cm = if st.nc == 0 { "discuss".to_string() } else { format!("{}&nbsp;comments", st.nc) };
                let hours = self.r.randint(1, 23);
                rows.push(format!(
                    r##"<tr class="athing" id="{id}"><td class="rank">{rank}.</td><td class="title"><span class="titleline"><a href="item-{id}.html">{t}</a>{site}</span></td></tr>
<tr><td></td><td class="subtext"><span class="score">{pts} points</span> by <a class="hnuser" href="#">{a}</a> <span class="age">{hours} hours ago</span> | <a href="item-{id}.html">{cm}</a></td></tr>
<tr class="spacer" style="height:5px"></tr>"##,
                    id = st.id,
                    t = e(&st.title),
                    pts = st.points,
                    a = st.author
                ));
                let cms: String = st
                    .comments
                    .iter()
                    .map(|c| format!(r##"<tr class="comtr"><td><div class="comhead"><a class="hnuser" href="#">{}</a> <span class="age">1 hour ago</span></div><div class="commtext">{}</div></td></tr>"##, c.author, e(&c.text)))
                    .collect();
                let head = format!(
                    r##"<table class="fatitem"><tr class="athing"><td class="title"><a href="#">{}</a></td></tr>
<tr><td class="subtext"><span class="score">{} points</span> by <a class="hnuser" href="#">{}</a></td></tr></table>"##,
                    e(&st.title),
                    st.points,
                    st.author
                );
                let body = if cms.is_empty() { format!("{head}<p>No comments yet.</p>") } else { format!("{head}\n<table class=\"comment-tree\">{cms}</table>") };
                self.write(&format!("forum/item-{}.html", st.id), &page(&st.title, &body, ""))?;
                rec.push((st.title.clone(), st.points.to_string(), st.author.clone(), st.comments.first().map(|c| c.author.clone())));
            }
            let more = if p < 3 { format!(r#"<tr><td></td><td class="title"><a href="news-{}.html" class="morelink" rel="next">More</a></td></tr>"#, p + 1) } else { String::new() };
            self.write(&format!("forum/news-{p}.html"), &page("Tech News", &format!(r#"<table class="itemlist">{}{more}</table>"#, rows.concat()), ""))?;
        }
        let comments = rec.iter().map(|(t, p, a, f)| J::Obj(vec![("title", s(t)), ("points", s(p)), ("author", s(a)), ("first_commenter", opt(f))])).collect();
        let list = rec.iter().map(|(t, p, a, _)| J::Obj(vec![("title", s(t)), ("points", s(p)), ("author", s(a))])).collect();
        self.gold("forum_comments", comments)?;
        self.gold("forum_list", list)
    }

    // ------------------------------------------------------------ 4. directory (3 levels)
    fn directory(&mut self) -> Result<()> {
        struct Biz {
            id: String,
            name: String,
            phone: String,
            web: Option<String>,
            street: String,
        }
        let cats = ["Bakeries", "Bike shops", "Bookstores", "Cafés", "Florists", "Plumbers"];
        let kinds = ["Bakery", "Cycles", "Books", "Café", "Flowers", "Plumbing"];
        let mut rec = Vec::new();
        let mut lis = Vec::new();
        for (ci, c) in cats.iter().enumerate() {
            let slug = format!("c{}", ci + 1);
            let n = self.r.randint(5, 23) as usize;
            let mut bs = Vec::new();
            for k in 0..n {
                let name = format!("{} {}", self.last(), kinds[ci]);
                let (a, b, cc, d) = (self.r.randint(20, 99), self.r.randint(10, 99), self.r.randint(10, 99), self.r.randint(10, 99));
                let phone = format!("+47 {a} {b} {cc} {d}");
                let host = self.last().to_lowercase();
                let web = self.r.choice(&[None, Some(format!("https://{host}-{k}.example.no"))]);
                let street = format!("{}gata {}", self.last(), self.r.randint(1, 90));
                bs.push(Biz { id: format!("biz-{slug}-{k}"), name, phone, web, street });
            }
            let pages = (n + 7) / 8;
            lis.push(format!(r#"<li class="cat"><a href="{slug}-1.html">{}</a> <span class="count">({n})</span></li>"#, e(c)));
            for p in 1..=pages {
                let items: String = chunk(&bs, p, 8)
                    .iter()
                    .map(|b| format!(r#"<li class="biz"><a href="{}.html">{}</a><br><small>{}</small></li>"#, b.id, e(&b.name), e(&b.street)))
                    .collect();
                let body = format!(r#"<p><a href="index.html">All categories</a></p><h1>{}</h1><ol class="businesses">{items}</ol>{}"#, e(c), pager(p, pages, |q| format!("{slug}-{q}.html")));
                self.write(&format!("directory/{slug}-{p}.html"), &page(c, &body, ""))?;
                for b in chunk(&bs, p, 8) {
                    let web = b.web.as_ref().map_or(String::new(), |w| format!(r#"<p>Website: <a href="{w}">{w}</a></p>"#));
                    let body = format!(r#"<h1>{}</h1><div class="contact"><p><b>Phone:</b> {}</p><p><b>Address:</b> {}, Oslo</p>{web}</div>"#, e(&b.name), b.phone, e(&b.street));
                    self.write(&format!("directory/{}.html", b.id), &page(&b.name, &body, ""))?;
                    rec.push(J::Obj(vec![("category", s(*c)), ("name", s(&b.name)), ("phone", s(&b.phone)), ("website", opt(&b.web))]));
                }
            }
        }
        self.write("directory/index.html", &page("Directory", &format!(r#"<h1>Oslo business directory</h1><ul class="categories">{}</ul>"#, lis.concat()), ""))?;
        self.gold("directory_all", rec)
    }

    // ------------------------------------------------------------ 5. spa (hash routes, client-rendered details)
    fn spa(&mut self) -> Result<()> {
        let mut homes = Vec::new();
        for i in 0..34 {
            let address = format!("{}veien {}", self.last(), self.r.randint(1, 120));
            let price = format!("kr {:.1} mill.", self.r.randint(28, 140) as f64 / 10.0).replace('.', ",");
            let beds = self.r.randint(1, 5);
            let agent = self.name();
            let area = format!("{} m²", self.r.randint(28, 220));
            homes.push(J::Obj(vec![("id", J::Int(700 + i)), ("address", s(address)), ("price", s(price)), ("beds", J::Int(beds)), ("agent", s(agent)), ("area", s(area))]));
        }
        let script = r#"<script>
const H = %s;
const app = document.querySelector('#app');
function list(p){
  const per = 10, pages = Math.ceil(H.length/per);
  const rows = H.slice((p-1)*per, p*per).map(h => `<li class="home" data-id="${h.id}"><div class="addr">${h.address}</div><div class="price">${h.price}</div><div class="beds">${h.beds} bedrooms</div></li>`).join('');
  const nav = Array.from({length: pages}, (_, i) => `<button class="pg${i+1===p?' on':''}" ${i+1===p?'aria-current="page"':''} data-p="${i+1}">${i+1}</button>`).join('') + (p < pages ? `<button class="pg" data-p="${p+1}" aria-label="Next page">›</button>` : '');
  app.innerHTML = `<h1>Homes for sale</h1><ul class="homes">${rows}</ul><div class="pages">${nav}</div>`;
  app.querySelectorAll('.home').forEach(li => li.onclick = () => location.hash = `#/home/${li.dataset.id}`);
  app.querySelectorAll('.pg').forEach(b => b.onclick = () => location.hash = `#/page/${b.dataset.p}`);
}
function detail(id){
  const h = H.find(x => x.id == id);
  app.innerHTML = `<button onclick="history.back()">Back</button><h1>${h.address}</h1><div class="info"><div><span class="k">Asking price</span> <span class="v">${h.price}</span></div><div><span class="k">Living area</span> <span class="v">${h.area}</span></div><div><span class="k">Agent</span> <span class="v">${h.agent}</span></div></div>`;
}
function route(){
  app.innerHTML = '<p>Loading…</p>';
  setTimeout(() => { const m = location.hash.match(/#\/(page|home)\/(\d+)/); if (m && m[1] === 'home') detail(+m[2]); else list(m ? +m[2] : 1); }, 250);
}
addEventListener('hashchange', route); route();
</script>"#
            .replacen("%s", &dumps(&J::Arr(homes.clone()), true), 1);
        self.write("spa/index.html", &page("Homes", r#"<div id="app"></div>"#, &script))?;
        let recs = homes
            .iter()
            .map(|h| {
                let J::Obj(kv) = h else { unreachable!() };
                let get = |k: &str| kv.iter().find(|(kk, _)| *kk == k).unwrap().1.clone();
                J::Obj(vec![("address", get("address")), ("price", get("price")), ("area", get("area")), ("agent", get("agent"))])
            })
            .collect();
        self.gold("spa_details", recs)
    }

    // ------------------------------------------------------------ 6. infinite scroll feed
    fn feed(&mut self) -> Result<()> {
        let mut posts = Vec::new();
        for i in 0..64 {
            let author = self.name();
            let verb = self.pick(&["Shipped", "Fixed", "Refactored", "Benchmarked", "Deleted"]);
            let what = self.pick(&["the parser", "our CI", "a race", "2k lines", "the cache"]);
            let likes = self.r.randint(0, 999);
            posts.push((author, format!("{verb} {what} today #{i}"), likes));
        }
        let data = J::Arr(posts.iter().map(|(a, t, l)| J::Obj(vec![("author", s(a)), ("text", s(t)), ("likes", J::Int(*l))])).collect());
        let script = r#"<script>
const P = %s; let n = 0, busy = false;
function load(){ if (busy || n >= P.length) return; busy = true; document.querySelector('#spin').hidden = false;
  setTimeout(() => { const f = document.querySelector('#feed');
    for (const p of P.slice(n, n + 16)) { const a = document.createElement('article'); a.className = 'post';
      a.innerHTML = `<header><strong class="who">${p.author}</strong></header><p class="body">${p.text}</p><footer><button class="like">♥ <span class="n">${p.likes}</span></button></footer>`; f.appendChild(a); }
    n += 16; busy = false; document.querySelector('#spin').hidden = true; if (n >= P.length) document.querySelector('#end').hidden = false; }, 350); }
addEventListener('scroll', () => { if (innerHeight + scrollY >= document.body.scrollHeight - 200) load(); });
load();
</script>"#
            .replacen("%s", &dumps(&data, true), 1);
        self.write("feed/index.html", &page("Feed", r#"<h1>Timeline</h1><div id="feed"></div><p id="spin" hidden>Loading…</p><p id="end" hidden>You're all caught up</p>"#, &script))?;
        self.gold("feed_all", posts.iter().map(|(a, t, l)| J::Obj(vec![("author", s(a)), ("text", s(t)), ("likes", s(l.to_string()))])).collect())
    }

    // ------------------------------------------------------------ 7. invoices table (filter)
    fn invoices(&mut self) -> Result<()> {
        struct Inv {
            no: String,
            cust: String,
            amount: String,
            due: String,
            status: String,
        }
        let custs = ["Halvorsen Freight AS", "Brightwater Mills", "Kobayashi Textiles", "Ostrava Cycle Works", "Meridian Apothecary", "Tallgrass Outfitters", "Vireo Ceramics"];
        let mut inv = Vec::new();
        for i in 0..87 {
            let cust = self.pick(&custs);
            let whole = self.r.randint(200, 90000);
            let cents = self.r.randint(0, 99);
            let (m, d) = (self.r.randint(1, 9), self.r.randint(1, 28));
            let status = self.pick(&["Paid", "Paid", "Overdue", "Open", "Draft"]);
            inv.push(Inv { no: format!("INV-26-{}", 4100 + i * 3), cust, amount: format!("{}.{cents:02} NOK", comma(whole)), due: format!("2026-{m:02}-{d:02}"), status });
        }
        let per = 20;
        let pages = inv.len().div_ceil(per);
        for p in 1..=pages {
            let rows: String = chunk(&inv, p, per)
                .iter()
                .map(|x| format!(r##"<tr><td><a href="#">{}</a></td><td>{}</td><td class="num">{}</td><td>{}</td><td><span class="pill {}">{}</span></td></tr>"##, x.no, e(&x.cust), x.amount, x.due, x.status.to_lowercase(), x.status))
                .collect();
            let next = if p < pages { format!(r#"<a href="p{}.html" aria-label="Next page">›</a>"#, p + 1) } else { String::new() };
            let body = format!(
                r#"<h1>Invoices</h1><table class="grid"><thead><tr><th>Invoice</th><th>Customer</th><th>Amount</th><th>Due</th><th>Status</th></tr></thead><tbody>{rows}</tbody></table>
<div class="pager">Page {p} of {pages} {next}</div>
<h3>Recent activity</h3><table class="log"><tr><td>Reminder sent</td><td>INV-26-4100</td></tr><tr><td>Payment received</td><td>INV-26-4103</td></tr><tr><td>Invoice voided</td><td>INV-26-4106</td></tr></table>"#
            );
            self.write(&format!("invoices/p{p}.html"), &page("Invoices", &body, ""))?;
        }
        let overdue = inv
            .iter()
            .filter(|x| x.status == "Overdue")
            .map(|x| J::Obj(vec![("invoice", s(&x.no)), ("customer", s(&x.cust)), ("amount", s(&x.amount)), ("due", s(&x.due))]))
            .collect();
        self.gold("invoices_overdue", overdue)
    }

    // ============================================================ held-out sites
    // ------------------------------------------------------------ H1. recipes (multi-link cards, /page/N)
    fn recipes(&mut self) -> Result<()> {
        struct Recipe {
            id: String,
            title: String,
            author: String,
            cat: String,
            prep: i64,
            serves: i64,
        }
        let dishes = ["Shakshuka", "Pad Thai", "Carbonara", "Ramen", "Tagine", "Risotto", "Pho", "Paella", "Moussaka", "Biryani", "Gnocchi", "Laksa", "Goulash", "Falafel"];
        let cats = ["Vegetarian", "Quick", "Comfort", "Spicy", "Weekend"];
        let mut rs = Vec::new();
        for i in 0..47 {
            let a = self.pick(&["Classic", "Easy", "Smoky", "Crispy", "Weeknight", "Lemony"]);
            let d = self.pick(&dishes);
            let author = self.name();
            let cat = self.pick(&cats);
            let prep = self.r.choice(&[10, 15, 20, 25, 30, 45, 60, 90]);
            let serves = self.r.randint(1, 8);
            rs.push(Recipe { id: format!("r{}", 500 + i), title: format!("{a} {d}"), author, cat, prep, serves });
        }
        let per = 10;
        let pages = rs.len().div_ceil(per);
        let href = |q: usize| if q == 1 { "/scrape/recipes/index.html".to_string() } else { format!("/scrape/recipes/page/{q}.html") };
        for p in 1..=pages {
            let cards: String = chunk(&rs, p, per)
                .iter()
                .map(|r| {
                    format!(
                        r#"<li class="recipe-card"><a class="tag" href="/scrape/recipes/tag/{}.html">{}</a>
<h2><a class="recipe-link" href="/scrape/recipes/r/{id}.html">{}</a></h2><p>by <a class="author" href="/scrape/recipes/u/{id}.html">{}</a></p></li>"#,
                        r.cat.to_lowercase(),
                        r.cat,
                        e(&r.title),
                        e(&r.author),
                        id = r.id
                    )
                })
                .collect();
            let path = if p == 1 { "recipes/index.html".to_string() } else { format!("recipes/page/{p}.html") };
            self.write(&path, &page("Recipes", &format!(r#"<h1>Latest recipes</h1><ul class="recipes">{cards}</ul>{}"#, pager(p, pages, href)), ""))?;
            for r in chunk(&rs, p, per) {
                let body = format!(
                    r#"<article><h1>{}</h1><p class="byline">By {}</p>
<ul class="facts"><li>Prep: <time datetime="PT{prep}M">{prep} min</time></li><li>Serves {}</li><li>Category: {}</li></ul>
<h2>Ingredients</h2><ul class="ingredients"><li>1 onion</li><li>2 cloves garlic</li><li>salt</li></ul></article>"#,
                    e(&r.title),
                    e(&r.author),
                    r.serves,
                    r.cat,
                    prep = r.prep
                );
                self.write(&format!("recipes/r/{}.html", r.id), &page(&r.title, &body, ""))?;
            }
        }
        let all = rs
            .iter()
            .map(|r| J::Obj(vec![("title", s(&r.title)), ("author", s(&r.author)), ("prep_time", s(format!("{} min", r.prep))), ("servings", s(r.serves.to_string()))]))
            .collect();
        self.gold("recipes_all", all)
    }

    // ------------------------------------------------------------ H2. issues (JS button pager, disabled at end, label filter)
    fn issues(&mut self) -> Result<()> {
        struct Issue {
            n: String,
            title: String,
            author: String,
            labels: Vec<&'static str>,
        }
        let labels = ["bug", "enhancement", "docs", "good first issue", "performance", "question"];
        let mut iss = Vec::new();
        for i in 0..64 {
            let k = self.r.randint(0, 3) as usize;
            let mut ls = self.r.sample(&labels, k);
            ls.sort();
            let a = self.pick(&["Crash when", "Slow render of", "Typo in", "Support for", "Flaky test in", "Memory leak in"]);
            let b = self.pick(&["the parser", "tables", "CLI help", "Firefox", "the pager", "big lists"]);
            let who = self.first().to_lowercase();
            let author = format!("{who}-{}", self.r.randint(10, 99));
            iss.push(Issue { n: format!("#{}", 2400 - i * 3), title: format!("{a} {b}"), author, labels: ls });
        }
        let per = 15;
        let pages = iss.len().div_ceil(per);
        for p in 1..=pages {
            let rows: String = chunk(&iss, p, per)
                .iter()
                .map(|x| {
                    let spans: String = x.labels.iter().map(|l| format!(r#"<span class="IssueLabel">{l}</span>"#)).collect();
                    format!(
                        r##"<div class="Box-row" role="listitem"><a class="Link--primary" href="#">{}</a> {spans}
<div class="opened-by">{} opened by <a class="author" href="#">{}</a></div></div>"##,
                        e(&x.title),
                        x.n,
                        x.author
                    )
                })
                .collect();
            let nxt = if p < pages { format!(r#"<button class="next_page" onclick="location.href='page{}.html'">Next</button>"#, p + 1) } else { r#"<button class="next_page" disabled>Next</button>"#.to_string() };
            let prv = if p > 1 { format!(r#"<button class="prev_page" onclick="location.href='page{}.html'">Previous</button>"#, p - 1) } else { r#"<button class="prev_page" disabled>Previous</button>"#.to_string() };
            self.write(&format!("issues/page{p}.html"), &page("Issues", &format!(r#"<h1>Issues</h1><div class="Box" role="list">{rows}</div><div class="paginate-container">{prv}{nxt}</div>"#), ""))?;
        }
        let bugs = iss
            .iter()
            .filter(|x| x.labels.contains(&"bug"))
            .map(|x| J::Obj(vec![("number", s(&x.n)), ("title", s(&x.title)), ("author", s(&x.author))]))
            .collect();
        self.gold("issues_bugs", bugs)
    }

    // ------------------------------------------------------------ H3. news (sections × load more × per-section limit)
    fn news(&mut self) -> Result<()> {
        let secs = ["World", "Tech", "Sports", "Culture"];
        let mut rec = Vec::new();
        let mut lis = Vec::new();
        for sname in secs {
            let low = sname.to_lowercase();
            let mut arts = Vec::new();
            for k in 0..self.r.randint(9, 14) {
                let what = self.pick(&["Talks resume", "Record set", "New study", "Merger announced", "Festival opens", "Storm warning", "Launch delayed"]);
                let head = format!("{sname}: {what} ({})", k + 1);
                let by = self.name();
                let date = format!("2026-09-{:02}", self.r.randint(1, 25));
                arts.push((format!("{low}-{k}"), head, by, date));
            }
            lis.push(format!(r#"<li class="section"><a href="{low}.html">{sname}</a></li>"#));
            let data = dumps(&J::Arr(arts.iter().map(|(id, head, _, _)| J::Obj(vec![("id", s(id)), ("head", s(head))])).collect()), true);
            let script = r#"<script>const A=%s;let n=0;function more(){const ul=document.querySelector('#arts');for(const a of A.slice(n,n+4)){const li=document.createElement('li');li.className='story';li.innerHTML=`<a href="a/${a.id}.html">${a.head}</a>`;ul.appendChild(li);}n+=4;if(n>=A.length)document.querySelector('#more').remove();}more();document.querySelector('#more').onclick=()=>setTimeout(more,300);</script>"#.replacen("%s", &data, 1);
            self.write(&format!("news/{low}.html"), &page(sname, &format!(r#"<p><a href="index.html">Front page</a></p><h1>{sname}</h1><ul id="arts"></ul><button id="more">Load more stories</button>"#), &script))?;
            for (id, head, by, date) in &arts {
                let body = format!(r#"<article><h1>{}</h1><p class="byline">By <span class="author">{}</span> · <time datetime="{date}">{date}</time></p><p>Body text.</p></article>"#, e(head), e(by));
                self.write(&format!("news/a/{id}.html"), &page(head, &body, ""))?;
            }
            rec.extend(arts.iter().take(5).map(|(_, head, by, date)| J::Obj(vec![("section", s(sname)), ("headline", s(head)), ("byline", s(by)), ("date", s(date))])));
        }
        self.write("news/index.html", &page("News", &format!(r#"<h1>The Daily Fixture</h1><ul class="sections">{}</ul>"#, lis.concat()), ""))?;
        self.gold("news_sections", rec)
    }

    // ------------------------------------------------------------ H4. deals (numeric filter + first-N across pages)
    fn deals(&mut self) -> Result<()> {
        let mut items = Vec::new();
        for _ in 0..70 {
            let b = self.pick(&["Aurora", "Nimbus", "Vertex", "Helix"]);
            let t = self.pick(&["Kettle", "Lamp", "Backpack", "Blender", "Jacket", "Tent", "Drone"]);
            let n = self.r.randint(2, 9);
            let price = self.r.choice(&[12.0, 19.0, 24.5, 35.0, 49.99, 50.0, 64.0, 89.0, 120.0, 249.0]);
            items.push((format!("{b} {t} {n}"), price));
        }
        let per = 12;
        let pages = items.len().div_ceil(per);
        for p in 1..=pages {
            let rows: String = chunk(&items, p, per)
                .iter()
                .map(|(n, pr)| format!(r#"<tr><td class="n">{}</td><td class="p">{}</td><td><button>Add</button></td></tr>"#, e(n), money(*pr)))
                .collect();
            let body = format!(r#"<h1>Deals</h1><table><thead><tr><th>Product</th><th>Price</th><th></th></tr></thead><tbody>{rows}</tbody></table><p>Page {p} of {pages}</p>{}"#, pager(p, pages, |q| format!("{q}.html")));
            self.write(&format!("deals/{p}.html"), &page("Deals", &body, ""))?;
        }
        let cheap = items.iter().filter(|(_, pr)| *pr < 50.0).take(20).map(|(n, pr)| J::Obj(vec![("name", s(n)), ("price", s(money(*pr)))])).collect();
        self.gold("deals_under50", cheap)
    }

    // ------------------------------------------------------------ H5. events (dl records, JS month pager)
    fn events(&mut self) -> Result<()> {
        let months = ["September 2026", "October 2026", "November 2026", "December 2026", "January 2027"];
        let days: Vec<i64> = (1..29).collect();
        let mut ev = Vec::new();
        for m in months {
            let k = self.r.randint(4, 9) as usize;
            let mut ds = self.r.sample(&days, k);
            ds.sort();
            let mon = &m.split_whitespace().next().unwrap()[..3];
            let mut list = Vec::new();
            for d in ds {
                let title = self.pick(&["Jazz night", "Book launch", "Rust meetup", "Film club", "Poetry slam", "Open studio"]) + &format!(" #{d}");
                let venue = self.pick(&["Blå", "Kulturhuset", "Deichman", "Vega Scene"]);
                list.push(J::Obj(vec![("date", s(format!("{mon} {d}"))), ("title", s(title)), ("venue", s(venue))]));
            }
            ev.push((m, list));
        }
        let data = J::Obj(ev.iter().map(|(m, l)| (*m, J::Arr(l.clone()))).collect());
        let script = r#"<script>const M=%s, K=Object.keys(M); let i=0;
function show(){ const m=K[i]; document.querySelector('#month').textContent=m;
  document.querySelector('#cal').innerHTML = M[m].map(e=>`<dt class="when">${e.date}</dt><dd class="what"><strong>${e.title}</strong> — <span class="venue">${e.venue}</span></dd>`).join('');
  document.querySelector('#nextm').disabled = i >= K.length-1; }
document.querySelector('#nextm').onclick=()=>{ if(i<K.length-1){ i++; document.querySelector('#cal').innerHTML=''; setTimeout(show,200);} };
show();</script>"#
            .replacen("%s", &dumps(&data, true), 1);
        self.write("events/index.html", &page("Events", r#"<h1>What's on</h1><h2 id="month"></h2><dl id="cal"></dl><button id="nextm" aria-label="Next month">Next month ›</button>"#, &script))?;
        let recs = ev.iter().take(3).flat_map(|(_, l)| l.iter().cloned()).collect();
        self.gold("events_3months", recs)
    }

    // ------------------------------------------------------------ scale: 1000 products × detail pages
    fn bigcat(&mut self) -> Result<()> {
        let makers = ["Acme", "Globex", "Initech", "Umbrella", "Soylent", "Tyrell", "Cyberdyne", "Wonka"];
        let mut ps = Vec::new();
        for i in 0..1000 {
            let a = self.pick(&["Widget", "Gadget", "Sprocket", "Gizmo", "Doohickey"]);
            let b = self.pick(&["Mk", "Series", "Model"]);
            let dollars = self.r.randint(2, 900);
            let cents = self.r.randint(0, 99);
            let maker = self.pick(&makers);
            let weight = format!("{} g", self.r.randint(50, 9000));
            ps.push((10000 + i, format!("{a} {b} {i}"), format!("${dollars}.{cents:02}"), maker, weight));
        }
        let per = 25;
        let pages = ps.len() / per;
        for p in 1..=pages {
            let rows: String = chunk(&ps, p, per).iter().map(|(id, name, price, _, _)| format!(r#"<li class="prod"><a href="item/{id}.html">{name}</a> <span class="price">{price}</span></li>"#)).collect();
            self.write(&format!("bigcat/p{p}.html"), &page("Catalog", &format!(r#"<h1>Catalog</h1><ul class="products">{rows}</ul>{}"#, pager(p, pages, |q| format!("p{q}.html"))), ""))?;
            for (id, name, price, maker, weight) in chunk(&ps, p, per) {
                let body = format!("<h1>{name}</h1><dl><dt>Manufacturer</dt><dd>{maker}</dd><dt>Weight</dt><dd>{weight}</dd><dt>Price</dt><dd>{price}</dd></dl>");
                self.write(&format!("bigcat/item/{id}.html"), &page(name, &body, ""))?;
            }
        }
        let all = ps.iter().map(|(_, n, pr, m, w)| J::Obj(vec![("name", s(n)), ("price", s(pr)), ("manufacturer", s(m)), ("weight", s(w))])).collect();
        self.gold("bigcat_all", all)
    }

    // ------------------------------------------------------------ threads (link choice, flat nesting, rich text, throttling)
    fn threads(&mut self) -> Result<()> {
        type Parts = Vec<(&'static str, String)>;
        #[derive(Clone)]
        struct Comment {
            author: String,
            text: Parts,
            dead: bool,
            level: usize,
        }
        struct Story {
            id: i64,
            title: String,
            job: bool,
            comments: Vec<(Comment, Vec<Comment>)>,
            ext: String,
        }
        let words = ["compilers", "databases", "typography", "sailing", "keyboards", "fermentation", "trains", "maps"];
        let mut stories = Vec::new();
        for i in 0..24 {
            let job = i == 9;
            let mut comments = Vec::new();
            if !job {
                for t in 0..self.r.choice(&[0, 1, 2, 3, 5, 8]) {
                    let who = self.first().to_lowercase();
                    let author = format!("{who}{}", self.r.randint(1, 99));
                    let quote = self.pick(&words);
                    let about = self.pick(&words);
                    let options: [Parts; 3] = [
                        vec![("i", format!("> Quoting the article on {quote}")), ("p", "I disagree with this part.".into()), ("p", "Second paragraph.".into())],
                        vec![("", format!("Plain comment {t} about {about}."))],
                        vec![("", "Starts plain".into()), ("p", "then a paragraph with a ".into()), ("a", "link".into()), ("", " inside.".into())],
                    ];
                    let text = self.r.choice(&options);
                    let dead = self.r.random() < 0.15;
                    let top = Comment { author, text, dead, level: 0 };
                    let mut replies = Vec::new();
                    for r in 0..self.r.randint(0, 3) as usize {
                        let who = self.last().to_lowercase();
                        let author = format!("{who}{}", self.r.randint(1, 9));
                        replies.push(Comment { author, text: vec![("", format!("Reply {r} to {}", top.author))], dead: false, level: 1 + (r % 2) });
                    }
                    comments.push((top, replies));
                }
            }
            let title = if job { "Acme (YC S26) is hiring a data engineer".to_string() } else { format!("A deep dive into {} ({i})", self.pick(&words)) };
            stories.push(Story { id: 900 + i, title, job, comments, ext: format!("https://example.org/post/{i}") });
        }
        // As rendered HTML.
        let body = |parts: &Parts| -> String {
            parts
                .iter()
                .map(|(tag, t)| match *tag {
                    "" => e(t),
                    "a" => format!(r#"<a href="https://example.org/ref">{}</a>"#, e(t)),
                    tag => format!("<{tag}>{}</{tag}>", e(t)),
                })
                .collect()
        };
        // As shown: a paragraph starts a new line.
        let text = |parts: &Parts| -> String {
            let mut out = String::new();
            for (tag, t) in parts {
                if *tag == "p" && !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
            out
        };
        let mut rows = Vec::new();
        let mut rec = Vec::new();
        for (k, st) in stories.iter().enumerate() {
            let sub = if st.job { String::new() } else { format!(r#" | <a href="item-{}.html">{}&nbsp;comments</a>"#, st.id, st.comments.len()) };
            let points = self.r.randint(2, 300);
            rows.push(format!(
                r##"<tr class="athing submission" id="{}"><td class="rank">{}.</td><td class="title"><span class="titleline"><a href="{}">{}</a></span></td></tr>
<tr><td></td><td class="subtext"><span class="subline">{points} points by <a class="hnuser" href="#">x{k}</a> <a href="#">2 hours ago</a>{sub}</span></td></tr>"##,
                st.id,
                k + 1,
                st.ext,
                e(&st.title)
            ));
            if st.job {
                continue;
            }
            let mut crows = Vec::new();
            for (top, replies) in &st.comments {
                for c in std::iter::once(top).chain(replies) {
                    let lvl = c.level;
                    crows.push(format!(
                        r##"<tr class="athing comtr"><td><table><tr><td class="ind" indent="{lvl}"><img src="s.gif" height="1" width="{}"></td>
<td class="default"><div><span class="comhead"><a class="hnuser" href="#">{}</a> <a href="#next" class="clicky" aria-hidden="true">next</a></span></div>
<div class="comment"><div class="commtext {}">{}</div><div class="reply"><a href="#">reply</a></div></div></td></tr></table></td></tr>"##,
                        40 * lvl,
                        c.author,
                        if c.dead { "c5a" } else { "c00" },
                        body(&c.text)
                    ));
                }
            }
            let page_body = format!(r#"<table class="fatitem"><tr><td><a href="{}">{}</a></td></tr></table><table class="comment-tree">{}</table>"#, st.ext, e(&st.title), crows.concat());
            self.write(&format!("threads/item-{}.html", st.id), &page(&st.title, &page_body, ""))?;
            rec.extend(st.comments.iter().take(3).map(|(t, _)| J::Obj(vec![("author", s(&t.author)), ("body", s(text(&t.text)))])));
        }
        self.write("threads/index.html", &page("Threads", &format!(r#"<table id="bigbox"><tbody>{}</tbody></table>"#, rows.concat()), ""))?;
        self.gold("threads_top3", rec)
    }

    // ------------------------------------------------------------ csr (detail pages at real URLs, rendered by script)
    fn csr(&mut self) -> Result<()> {
        let mut xs = Vec::new();
        for i in 0..14 {
            let a = self.pick(&["Linden", "Birch", "Rowan", "Aspen"]);
            let b = self.pick(&["Chair", "Table", "Shelf", "Lamp"]);
            let color = self.pick(&["Oak", "Walnut", "Ash", "Black"]);
            let stock = self.r.randint(0, 40);
            xs.push((format!("k{i}"), format!("{a} {b} {i}"), color, stock));
        }
        let lis: String = xs.iter().map(|(id, name, _, _)| format!(r#"<li class="prod"><a href="p/{id}.html">{name}</a></li>"#)).collect();
        self.write("csr/index.html", &page("Furniture", &format!(r#"<h1>Furniture</h1><ul class="catalog">{lis}</ul>"#), ""))?;
        for (id, name, color, stock) in &xs {
            let data = dumps(&J::Obj(vec![("name", s(name)), ("color", s(color)), ("stock", J::Int(*stock))]), true);
            let script = r#"<script>const X=%s;setTimeout(()=>{document.querySelector('#app').innerHTML=`<h1>${X.name}</h1><dl><dt>Finish</dt><dd>${X.color}</dd><dt>In stock</dt><dd>${X.stock}</dd></dl>`},250);</script>"#.replacen("%s", &data, 1);
            self.write(&format!("csr/p/{id}.html"), &page("Loading", r#"<div id="app"></div>"#, &script))?;
        }
        let recs = xs.iter().map(|(_, n, c, st)| J::Obj(vec![("name", s(n)), ("finish", s(c)), ("in_stock", s(st.to_string()))])).collect();
        self.gold("csr_details", recs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::hash::{DefaultHasher, Hash, Hasher};

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fab-scrape-gen-{}-{tag}", std::process::id()))
    }

    fn hash_tree(root: &Path) -> BTreeMap<PathBuf, u64> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let mut h = DefaultHasher::new();
                    std::fs::read(&p).unwrap().hash(&mut h);
                    out.insert(p.strip_prefix(root).unwrap().to_path_buf(), h.finish());
                }
            }
        }
        out
    }

    #[test]
    fn generation_is_deterministic() {
        let (a, b) = (scratch("a"), scratch("b"));
        let ca = generate(&a.join("fixtures"), &a.join("gold")).unwrap();
        let cb = generate(&b.join("fixtures"), &b.join("gold")).unwrap();
        assert_eq!(ca, cb);
        assert_eq!(ca.len(), 20);
        let (ha, hb) = (hash_tree(&a), hash_tree(&b));
        assert!(ha.len() > 1000);
        assert_eq!(ha, hb);
        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
    }

    #[test]
    fn python_number_formatting() {
        assert_eq!(money(1624.875), "$1,624.88"); // exact tie, half-to-even
        assert_eq!(money(24.375), "$24.38");
        assert_eq!(money(0.125), "$0.12");
        assert_eq!(money(19.0), "$19.00");
        assert_eq!(comma(90000), "90,000");
        assert_eq!(comma(200), "200");
        assert_eq!(round2(2.675), 2.67);
        assert_eq!(float_repr(4.0), "4.0");
        assert_eq!(float_repr(4.2), "4.2");
        assert_eq!(float_g(5.0), "5");
        assert_eq!(dumps(&s("Blå m² \"q\""), true), concat!("\"Bl", "\\", "u00e5 m", "\\", "u00b2 \\\"q\\\"\""));
        assert_eq!(dumps(&s("€ – x"), false), "\"€ – x\"");
        assert_eq!(e(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;");
    }
}
