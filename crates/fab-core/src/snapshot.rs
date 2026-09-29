//! Page snapshot model, compact line representation, and lexical pruning.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct El {
    pub i: usize,
    /// role
    pub r: String,
    /// accessible name
    pub n: String,
    /// current value (textbox, select)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<String>,
    /// select options
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub o: Option<Vec<String>>,
    /// flags: disabled checked expanded covered ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f: Option<String>,
    /// container context
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
    /// input type / subtype
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    /// radio group
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g: Option<String>,
    /// placeholder
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<String>,
    /// record (row/list item/card) this element belongs to
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rc: Option<u32>,
    /// latent: key of the visible control that reveals this element
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rv: Option<u32>,
    /// looks like a pager control (Next, Load more, ...)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Text {
    pub i: usize,
    pub x: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<u8>,
    /// alert / live region
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<u8>,
    /// table row id, shared by the cells of one row
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<u32>,
    /// record (row/list item/card) this text belongs to
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rc: Option<u32>,
}

/// A repeated container: table row, list item, card, feed entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub k: u32,
    pub coll: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Snapshot {
    /// The element in `self` that is the same control as `old` from `prev`
    /// (same role, name, context and record label), when exactly one matches.
    /// Used when a re-render replaced the node a plan was going to act on.
    pub fn refind(&self, old: &El, prev: &Snapshot) -> Option<usize> {
        let label = |s: &Snapshot, e: &El| e.rc.and_then(|k| s.records.iter().find(|r| r.k == k)).and_then(|r| r.label.clone());
        let want = label(prev, old);
        let mut m = self.els.iter().filter(|e| e.r == old.r && e.n == old.n && e.c == old.c && label(self, e) == want && !e.latent());
        let first = m.next()?;
        m.next().is_none().then_some(first.i)
    }
}

/// A group of records (table, list, grid), with its columns and pager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coll {
    pub k: u32,
    #[serde(default)]
    pub n: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<Vec<String>>,
    /// key of the control that loads the next page/batch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(rename = "docId")]
    pub doc_id: String,
    pub version: u64,
    pub url: String,
    pub title: String,
    pub els: Vec<El>,
    pub texts: Vec<Text>,
    #[serde(default)]
    pub records: Vec<Record>,
    #[serde(default)]
    pub colls: Vec<Coll>,
    #[serde(default)]
    pub modal: bool,
    /// Accessible name of the open modal, if any.
    #[serde(default, rename = "modalName", skip_serializing_if = "Option::is_none")]
    pub modal_name: Option<String>,
    #[serde(default)]
    pub ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Select,
    Check,
    Radio,
    Click,
    Other,
}

impl El {
    pub fn id(&self) -> String {
        format!("e{}", self.i)
    }

    pub fn has_flag(&self, f: &str) -> bool {
        self.f.as_deref().is_some_and(|s| s.split(' ').any(|x| x == f))
    }

    /// Present in the DOM but hidden behind a reveal (collapsed menu, closed details, other tab).
    pub fn latent(&self) -> bool {
        self.rv.is_some()
    }

    pub fn kind(&self) -> Kind {
        let input = self.t.as_deref() == Some("input");
        match self.r.as_str() {
            // Readonly inputs are usually picker triggers (dates, comboboxes): click them.
            "textbox" | "searchbox" if self.has_flag("readonly") => Kind::Click,
            "textbox" => Kind::Text,
            "searchbox" | "combobox" if input => Kind::Text,
            "select" => Kind::Select,
            "checkbox" | "switch" => Kind::Check,
            "radio" => Kind::Radio,
            "slider" | "file" | "spinbutton" => Kind::Other,
            _ => Kind::Click,
        }
    }

    /// Compact one-line description, e.g.
    /// `button "Add to cart" — in: Blue Oxford Shirt`.
    pub fn desc(&self, ctx: bool) -> String {
        let mut s = String::with_capacity(64);
        s.push_str(&self.r);
        if let Some(t) = &self.t {
            s.push(':');
            s.push_str(t);
        }
        s.push_str(" \"");
        s.push_str(&self.n);
        s.push('"');
        if let Some(v) = &self.v {
            if matches!(self.kind(), Kind::Text | Kind::Select) || !v.is_empty() {
                s.push_str(&format!(" value=\"{v}\""));
            }
        }
        if let Some(p) = &self.p {
            s.push_str(&format!(" placeholder=\"{p}\""));
        }
        if let Some(o) = &self.o {
            let shown: Vec<&str> = o.iter().take(12).map(String::as_str).collect();
            s.push_str(&format!(" options=[{}", shown.join(" | ")));
            if o.len() > 12 {
                s.push_str(&format!(" | …+{}", o.len() - 12));
            }
            s.push(']');
        }
        if let Some(g) = &self.g {
            s.push_str(&format!(" group=\"{g}\""));
        }
        if let Some(f) = &self.f {
            let f: Vec<&str> = f.split(' ').filter(|x| *x != "latent").collect();
            if !f.is_empty() {
                s.push_str(&format!(" ({})", f.join(" ")));
            }
        }
        if let Some(rv) = self.rv {
            s.push_str(&format!(" (hidden until e{rv} is clicked)"));
        }
        if ctx {
            if let Some(c) = &self.c {
                s.push_str(" — in: ");
                s.push_str(c);
            }
        }
        s
    }

    pub fn line(&self, ctx: bool) -> String {
        format!("{} {}", self.id(), self.desc(ctx))
    }

    fn search_text(&self) -> String {
        let mut s = format!("{} {} {}", self.r, self.n, self.v.as_deref().unwrap_or(""));
        if let Some(c) = &self.c {
            s.push(' ');
            s.push_str(c);
        }
        if let Some(p) = &self.p {
            s.push(' ');
            s.push_str(p);
        }
        if let Some(o) = &self.o {
            for x in o.iter().take(40) {
                s.push(' ');
                s.push_str(x);
            }
        }
        s
    }
}

impl Text {
    pub fn id(&self) -> String {
        format!("t{}", self.i)
    }

    /// Text with heading marker but without row/col annotations.
    pub fn desc_plain(&self) -> String {
        match self.h {
            Some(h) => format!("[h{h}] {}", self.x),
            None => self.x.clone(),
        }
    }

    pub fn desc(&self) -> String {
        let mut s = String::new();
        if let Some(h) = self.h {
            s.push_str(&format!("[h{h}] "));
        }
        if self.a.is_some() {
            s.push_str("[alert] ");
        }
        s.push_str(&self.x);
        if let Some(c) = &self.c {
            s.push_str(&format!(" ({c})"));
        }
        s
    }
}

const STOP: &[&str] = &[
    "a", "an", "the", "to", "of", "in", "on", "for", "and", "or", "with", "into", "as", "at", "by", "is", "it", "this",
    "that", "then", "from", "my", "me", "i", "you", "your", "be", "please", "click", "press", "tap", "select", "choose",
    "type", "enter", "fill", "set", "go", "open", "field", "button", "link", "page",
];

pub fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !STOP.contains(&w.as_str()))
        .collect()
}

/// BM25-ish relevance of each document to the query. Returns scores in input order.
pub fn score_all(query: &str, docs: &[String]) -> Vec<f64> {
    let q: HashSet<String> = tokens(query).into_iter().collect();
    if q.is_empty() {
        return vec![0.0; docs.len()];
    }
    let toks: Vec<Vec<String>> = docs.iter().map(|d| tokens(d)).collect();
    let n = docs.len().max(1) as f64;
    let mut df: HashMap<&str, usize> = HashMap::new();
    for t in &toks {
        let uniq: HashSet<&str> = t.iter().map(String::as_str).collect();
        for w in uniq {
            if q.contains(w) {
                *df.entry(w).or_default() += 1;
            }
        }
    }
    let avg = toks.iter().map(Vec::len).sum::<usize>() as f64 / n;
    toks.iter()
        .map(|t| {
            let len = t.len() as f64;
            let mut tf: HashMap<&str, usize> = HashMap::new();
            for w in t {
                if q.contains(w.as_str()) {
                    *tf.entry(w).or_default() += 1;
                }
            }
            tf.iter()
                .map(|(w, &f)| {
                    let idf = ((n - df[w] as f64 + 0.5) / (df[w] as f64 + 0.5) + 1.0).ln();
                    let f = f as f64;
                    idf * f * 2.2 / (f + 1.2 * (0.25 + 0.75 * len / avg.max(1.0)))
                })
                .sum()
        })
        .collect()
}

/// Indices of the top-k items by score, returned in original (document) order.
/// Ties and zero scores keep document order, so small pages pass through intact.
pub fn top_k(scores: &[f64], k: usize) -> Vec<usize> {
    if scores.len() <= k {
        return (0..scores.len()).collect();
    }
    let mut idx: Vec<usize> = (0..scores.len()).collect();
    idx.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    idx.truncate(k);
    idx.sort_unstable();
    idx
}

impl Snapshot {
    /// Elements relevant to `query`, at most `k`, in document order.
    pub fn prune_els(&self, query: &str, k: usize, filter: impl Fn(&El) -> bool) -> Vec<&El> {
        let cand: Vec<&El> = self.els.iter().filter(|e| filter(e)).collect();
        let docs: Vec<String> = cand.iter().map(|e| e.search_text()).collect();
        let scores = score_all(query, &docs);
        top_k(&scores, k).into_iter().map(|i| cand[i]).collect()
    }

    /// Elements that actually match `query`, best first (no zero-score padding).
    pub fn search_els(&self, query: &str, k: usize, filter: impl Fn(&El) -> bool) -> Vec<&El> {
        let cand: Vec<&El> = self.els.iter().filter(|e| filter(e)).collect();
        let docs: Vec<String> = cand.iter().map(|e| e.search_text()).collect();
        let scores = score_all(query, &docs);
        let mut idx: Vec<usize> = (0..cand.len()).filter(|i| scores[*i] > 0.0).collect();
        idx.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b)));
        idx.into_iter().take(k).map(|i| cand[i]).collect()
    }

    pub fn prune_texts(&self, query: &str, k: usize) -> Vec<&Text> {
        let docs: Vec<String> = self.texts.iter().map(|t| format!("{} {}", t.x, t.c.as_deref().unwrap_or(""))).collect();
        let scores = score_all(query, &docs);
        top_k(&scores, k).into_iter().map(|i| &self.texts[i]).collect()
    }

    /// Short text context for a decision: headings, alerts, then the most relevant blocks.
    pub fn text_context(&self, query: &str, k: usize) -> Vec<String> {
        if k == 0 {
            return vec![];
        }
        let docs: Vec<String> = self.texts.iter().map(|t| t.x.clone()).collect();
        let scores = score_all(query, &docs);
        let mut pick: Vec<usize> = self
            .texts
            .iter()
            .enumerate()
            .filter(|(_, t)| t.a.is_some() || t.h.is_some_and(|h| h <= 2))
            .map(|(i, _)| i)
            .take(k / 2)
            .collect();
        for i in top_k(&scores, k) {
            if pick.len() >= k {
                break;
            }
            if scores[i] > 0.0 && !pick.contains(&i) {
                pick.push(i);
            }
        }
        pick.sort_unstable();
        pick.into_iter().map(|i| truncate(&self.texts[i].desc(), 160)).collect()
    }

    /// What's new relative to `before` (same document): texts whose node is new
    /// or whose content changed, with every touched record shown as one whole
    /// line. Diffing by node key keeps repeated strings (identical feed actions,
    /// equal prices) that a string diff would drop. Empty when nothing changed.
    pub fn changes_since(&self, before: &Snapshot, max_chars: usize) -> String {
        let old: HashMap<usize, &str> = before.texts.iter().map(|t| (t.i, t.x.as_str())).collect();
        let is_new = |t: &Text| old.get(&t.i).is_none_or(|x| *x != t.x);
        let touched: HashSet<u32> = self.texts.iter().filter(|t| is_new(t)).filter_map(|t| t.rc).collect();
        let mut out = String::new();
        for line in self.text_lines(|t| is_new(t) || t.rc.is_some_and(|r| touched.contains(&r))) {
            if out.len() + line.len() > max_chars {
                out.push_str("…(more; use read)\n");
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Text as display lines: the texts of one record are joined into a single
    /// line ("2026-09-19 14:09 · Tomasz Wrona revoked API key 'ci-runner'"),
    /// so a row can't be misread as belonging to its neighbour.
    pub fn text_lines<'a>(&'a self, keep: impl Fn(&Text) -> bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut cur: Option<u32> = None;
        for t in self.texts.iter().filter(|t| keep(t)) {
            let s = truncate(&t.desc_plain(), 300);
            match (t.rc, cur) {
                (Some(r), Some(c)) if r == c => {
                    let last = out.last_mut().unwrap();
                    last.push_str(" · ");
                    last.push_str(&s);
                }
                _ => {
                    out.push(if t.a.is_some() { format!("[alert] {s}") } else { s });
                    cur = t.rc;
                }
            }
        }
        out
    }

    /// Human/LLM-facing page summary.
    /// Title, address and text, without the element listing.
    pub fn brief(&self, max_text_chars: usize) -> String {
        let mut s = format!("{} — {}\n", self.title, self.url);
        let mut used = 0;
        for line in self.text_lines(|_| true) {
            if used > max_text_chars {
                s.push_str("…\n");
                break;
            }
            used += line.len();
            s.push_str(&line);
            s.push('\n');
        }
        s
    }

    pub fn summary(&self, max_els: usize, max_text_chars: usize) -> String {
        self.summary_first(max_els, max_text_chars, &HashSet::new())
    }

    /// `summary`, listing first the elements in `first` (e.g. those an action
    /// just revealed) and, while a modal is open, the ones not behind it. A
    /// dialog or panel is usually appended last in the page, so in page order
    /// its buttons fell past the element cap ("Create filter" after 60 others).
    pub fn summary_first(&self, max_els: usize, max_text_chars: usize, first: &HashSet<usize>) -> String {
        let mut s = format!("# {}\n{}\n", self.title, self.url);
        let mut used = 0;
        for line in self.text_lines(|_| true) {
            if used > max_text_chars {
                s.push_str("…\n");
                break;
            }
            used += line.len();
            s.push_str(&line);
            s.push('\n');
        }
        s.push_str("## elements\n");
        let mut shown: Vec<&El> = self.els.iter().filter(|e| !e.latent()).collect();
        // Stable: page order within each group.
        shown.sort_by_key(|e| (!first.contains(&e.i), self.modal && e.has_flag("covered")));
        for e in shown.iter().take(max_els) {
            s.push_str(&e.line(true));
            s.push('\n');
        }
        if shown.len() > max_els {
            s.push_str(&format!("… {} more elements; find one with observe(\"<name as written on the page>\")\n", shown.len() - max_els));
        }
        s
    }
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_k_keeps_doc_order() {
        let s = [0.0, 5.0, 1.0, 5.0, 0.0];
        assert_eq!(top_k(&s, 2), vec![1, 3]);
        assert_eq!(top_k(&s, 10), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn scoring_prefers_matching_rows() {
        let docs = vec![
            "button Delete invoice 1041".to_string(),
            "button Delete invoice 1042".to_string(),
            "link Home".to_string(),
        ];
        let s = score_all("delete invoice #1042", &docs);
        assert!(s[1] > s[0] && s[0] > s[2]);
    }
}
