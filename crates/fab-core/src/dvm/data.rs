//! Data layer: Jev decides which column and value a condition means; code
//! filters exactly and does the arithmetic. One Jev round per collect, however
//! many pages: every bind is asked speculatively for every column at once.

use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

use crate::jev::{Answers, Jev, Questions};
use crate::snapshot::{Snapshot, truncate};

/// One row: its cells by column name, and its verbatim text line.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub cells: Vec<(String, String)>,
    pub line: String,
}

impl Row {
    pub fn get(&self, col: &str) -> Option<&str> {
        self.cells.iter().find(|(c, _)| c == col).map(|(_, v)| v.as_str())
    }
}

/// Rows of the best collection on one page, plus its pager key.
pub struct PageRows {
    pub cols: Vec<String>,
    pub rows: Vec<Row>,
    pub next: Option<usize>,
    pub coll: u32,
}

/// Extracts the rows of collection `coll` (or the largest one) from a snapshot.
pub fn page_rows(snap: &Snapshot, coll: Option<u32>) -> Option<PageRows> {
    let c = match coll {
        Some(k) => snap.colls.iter().find(|c| c.k == k)?,
        None => snap.colls.iter().max_by_key(|c| c.n)?,
    };
    let cols: Vec<String> = c.cols.clone().unwrap_or_default();
    let mut rows = Vec::new();
    for r in snap.records.iter().filter(|r| r.coll == c.k) {
        let texts: Vec<&str> = snap.texts.iter().filter(|t| t.rc == Some(r.k)).map(|t| t.x.as_str()).collect();
        if texts.is_empty() {
            continue;
        }
        let cells: Vec<(String, String)> = texts
            .iter()
            .enumerate()
            .map(|(i, x)| (cols.get(i).cloned().unwrap_or_else(|| format!("field {}", i + 1)), x.to_string()))
            .collect();
        rows.push(Row { cells, line: texts.join(" · ") });
    }
    // The pager must be present, enabled and not a latent element.
    let next = c.next.map(|n| n as usize).filter(|n| snap.els.iter().any(|e| e.i == *n && !e.has_flag("disabled") && !e.latent()));
    Some(PageRows { cols, rows, next, coll: c.k })
}

/// The first numeric token of `s` and the text right after it. Thousands
/// separators (",", ".", or a space before exactly three digits) stay inside
/// the token; anything else ends it, so "16 GB LPDDR5X" is 16, not 165.
fn first_number(s: &str) -> Option<(String, bool, &str)> {
    let cs: Vec<(usize, char)> = s.char_indices().collect();
    let start = cs.iter().position(|(_, c)| c.is_ascii_digit())?;
    // A minus sign before the digits (possibly across a currency symbol) negates.
    let neg = cs[..start].iter().rev().take_while(|(_, c)| !c.is_alphanumeric()).any(|(_, c)| *c == '-' || *c == '\u{2212}');
    let mut t = String::new();
    let mut i = start;
    while i < cs.len() {
        let c = cs[i].1;
        if c.is_ascii_digit() {
            t.push(c);
        } else if (c == '.' || c == ',') && cs.get(i + 1).is_some_and(|(_, d)| d.is_ascii_digit()) {
            t.push(c);
        } else if (c == ' ' || c == '\u{a0}' || c == '\u{202f}')
            && cs.len() > i + 3
            && cs[i + 1..i + 4].iter().all(|(_, d)| d.is_ascii_digit())
            && cs.get(i + 4).is_none_or(|(_, d)| !d.is_ascii_digit())
            && t.len() <= 3
            && !t.contains(['.', ','])
        {
            // "1 290,5": a space-separated thousands group.
        } else {
            break;
        }
        i += 1;
    }
    let rest = cs.get(i).map(|(b, _)| &s[*b..]).unwrap_or("");
    Some((t, neg, rest))
}

/// A quantity with its unit normalized, so "512 MB" < "16 GB" < "1 TB" and
/// "90 min" < "2 h". Falls back to the bare number.
pub fn quantity(s: &str) -> Option<f64> {
    let v = number(s)?;
    let (_, _, rest) = first_number(s)?;
    let unit: String = rest.trim_start().chars().take_while(|c| c.is_alphabetic()).collect::<String>().to_lowercase();
    let scale = match unit.as_str() {
        "kb" => 1.0 / 1024.0,
        "mb" => 1.0,
        "gb" => 1024.0,
        "tb" => 1024.0 * 1024.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1.0 / 60.0,
        "min" | "mins" | "minute" | "minutes" => 1.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60.0,
        "g" | "gram" | "grams" => 1.0,
        "kg" | "kilogram" | "kilograms" => 1000.0,
        _ => 1.0,
    };
    Some(v * scale)
}

/// Parses "$1,290.00", "1 290,5", "42.50 USD" → 1290.0 / 1290.5 / 42.5, and
/// only the first number: "16 GB LPDDR5X" → 16.
pub fn number(s: &str) -> Option<f64> {
    let (t, neg, _) = first_number(s)?;
    let t = if neg { format!("-{t}") } else { t };
    // With both separators the last one is the decimal point ("1.290,50" /
    // "1,290.50"); a lone comma not followed by exactly three digits is a
    // decimal comma ("1290,5"); repeated dots are thousands ("1.290.000").
    let (dot, comma) = (t.rfind('.'), t.rfind(','));
    let t = if let (Some(d), Some(c)) = (dot, comma) {
        if c > d { t.replace('.', "").replace(',', ".") } else { t.replace(',', "") }
    } else if t.matches('.').count() > 1 {
        t.replace('.', "")
    } else if t.contains('.') {
        t
    } else if t.matches(',').count() == 1 && t.rsplit(',').next()?.len() != 3 {
        t.replace(',', ".")
    } else {
        t.replace(',', "")
    };
    t.parse().ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
}

/// A numeric condition parsed in code: "under $1,500" → (Lt, 1500).
pub fn numeric_cond(s: &str) -> Option<(Cmp, f64)> {
    let l = s.to_lowercase();
    let pats: [(&str, Cmp); 12] = [
        ("at most", Cmp::Le),
        ("no more than", Cmp::Le),
        ("at least", Cmp::Ge),
        ("no less than", Cmp::Ge),
        ("less than", Cmp::Lt),
        ("more than", Cmp::Gt),
        ("greater than", Cmp::Gt),
        ("cheaper than", Cmp::Lt),
        ("under", Cmp::Lt),
        ("below", Cmp::Lt),
        ("over", Cmp::Gt),
        ("above", Cmp::Gt),
    ];
    for (p, c) in pats {
        if let Some(i) = l.find(p) {
            return number(&l[i + p.len()..]).map(|v| (c, v));
        }
    }
    for (p, c) in [("<=", Cmp::Le), (">=", Cmp::Ge), ("≤", Cmp::Le), ("≥", Cmp::Ge), ("<", Cmp::Lt), (">", Cmp::Gt)] {
        if let Some(i) = l.find(p) {
            return number(&l[i + p.len()..]).map(|v| (c, v));
        }
    }
    None
}

/// Splits "status is Refunded and customer is Linnea Dahlqvist" into conjuncts.
pub fn conjuncts(w: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in w.split(" and ").flat_map(|p| p.split(", ")).flat_map(|p| p.split(';')) {
        let p = part.trim();
        if !p.is_empty() {
            out.push(p.to_string());
        }
    }
    out
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Filter {
    pub cond: String,
    pub col: Option<String>,
    /// Exact value the column must equal (Jev-bound).
    pub eq: Option<String>,
    /// The column values that satisfy the condition (Jev yes/no per distinct
    /// value): handles multi-valued cells ("billing, refund"), synonyms and
    /// categories that no single value names. Takes precedence over `eq`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept: Option<Vec<String>>,
    /// Numeric comparison (parsed in code).
    pub num: Option<(Cmp, f64)>,
}

/// One speculative round: for each conjunct, which column; for each (conjunct,
/// column), which distinct value; plus the aggregate column. Code picks the
/// answers that apply.
pub async fn bind(
    jev: &Jev,
    what: &str,
    cols: &[String],
    rows: &[Row],
    conds: &[String],
    of: Option<&str>,
    by: Option<&str>,
) -> Result<(Vec<Filter>, Option<String>, Option<String>, Vec<Answers>)> {
    if cols.is_empty() {
        bail!("this collection has no named columns");
    }
    let mut q = Questions::default();
    let col_opts = |none: &str| {
        cols.iter().map(|c| (c.clone(), None)).chain(std::iter::once((none.to_string(), None))).collect::<Vec<_>>()
    };
    let distinct = |c: &str| -> Vec<String> {
        let s: BTreeSet<&str> = rows.iter().filter_map(|r| r.get(c)).collect();
        s.into_iter().map(str::to_string).collect()
    };
    for (i, cond) in conds.iter().enumerate() {
        q.choice(
            format!("col{i}"),
            json!({"condition": cond, "question": "Which column of `rows` does `condition` test?"}),
            col_opts("(none of these)"),
        );
        if numeric_cond(cond).is_some() {
            continue;
        }
        for (j, c) in cols.iter().enumerate() {
            let vals = distinct(c);
            if vals.len() < 2 || vals.len() > 254 {
                continue;
            }
            let opts = vals.into_iter().map(|v| (v, None)).chain(std::iter::once(("(none of these)".to_string(), None)));
            q.choice(
                format!("val{i}_{j}"),
                json!({"condition": cond, "column": c, "question": "Which value of `column` does `condition` select?"}),
                opts,
            );
        }
    }
    // Oracle: a requested name that is literally a column needs no question.
    let exact = |want: &str| cols.iter().find(|c| c.trim().eq_ignore_ascii_case(want.trim())).cloned();
    let (of_exact, by_exact) = (of.and_then(exact), by.and_then(exact));
    // Neutral wording: "which column holds `quantity`" made Jev pick "Items".
    if let (Some(of), None) = (of, &of_exact) {
        q.choice("of", json!({"wanted": of, "question": "Which column of `rows` contains the `wanted` values?"}), col_opts("(none of these)"));
    }
    if let (Some(by), None) = (by, &by_exact) {
        q.choice("by", json!({"wanted": by, "question": "Which column of `rows` contains the `wanted` values?"}), col_opts("(none of these)"));
    }
    let state = json!({
        "collection": what,
        "columns": cols,
        "rows": rows.iter().take(8).map(|r| r.cells.iter().map(|(c, v)| format!("{c}: {v}")).collect::<Vec<_>>().join(" | ")).collect::<Vec<_>>(),
    });
    let a = jev.ask(&state, &q).await?;
    let pick = |id: &str| a.choice(id).map(|(c, _, _)| c.to_string()).filter(|c| c != "(none of these)");
    let mut filters = Vec::new();
    for (i, cond) in conds.iter().enumerate() {
        let col = pick(&format!("col{i}"));
        let num = numeric_cond(cond);
        let eq = match (&col, num) {
            (Some(c), None) => cols.iter().position(|x| x == c).and_then(|j| pick(&format!("val{i}_{j}"))),
            _ => None,
        };
        filters.push(Filter { cond: cond.clone(), col, eq, num, accept: None });
    }
    let of_col = of_exact.or_else(|| of.and_then(|_| pick("of")));
    let by_col = by_exact.or_else(|| by.and_then(|_| pick("by")));
    let mut answers = vec![a];
    // Round 2: which values of the bound column satisfy each condition. A
    // single bound value undercounts whenever several values qualify.
    let mut q2 = Questions::default();
    let mut asked: Vec<(usize, Vec<String>)> = Vec::new();
    for (i, f) in filters.iter().enumerate() {
        let (Some(c), None) = (&f.col, f.num) else { continue };
        let vals = distinct(c);
        if vals.is_empty() || vals.len() > 120 || asked.iter().map(|(_, v)| v.len()).sum::<usize>() + vals.len() > 240 {
            continue;
        }
        for (k, v) in vals.iter().enumerate() {
            q2.noul_criteria(
                format!("in{i}_{k}"),
                json!({"condition": f.cond, "column": c, "value": v, "question": "Does a row whose `column` is `value` satisfy `condition`?"}),
                "The value satisfies the condition (for a list of values, one of them does).",
                "The value does not satisfy the condition.",
            );
        }
        asked.push((i, vals));
    }
    if !q2.is_empty() {
        let state = json!({"collection": what});
        let a2 = jev.ask(&state, &q2).await?;
        for (i, vals) in &asked {
            let acc: Vec<String> = vals.iter().enumerate().filter(|(k, _)| a2.yes(&format!("in{i}_{k}")).unwrap_or(0.0) >= 0.5).map(|(_, v)| v.clone()).collect();
            filters[*i].accept = Some(acc);
        }
        answers.push(a2);
    }
    Ok((filters, of_col, by_col, answers))
}

pub fn matches(r: &Row, f: &Filter) -> bool {
    let Some(col) = &f.col else { return true };
    let Some(v) = r.get(col) else { return false };
    if let Some(acc) = &f.accept {
        return acc.iter().any(|a| a == v);
    }
    if let Some(eq) = &f.eq {
        return v == eq;
    }
    if let (Some((cmp, x)), Some(y)) = (f.num, number(v)) {
        return match cmp {
            Cmp::Lt => y < x,
            Cmp::Le => y <= x,
            Cmp::Gt => y > x,
            Cmp::Ge => y >= x,
        };
    }
    // A condition Jev couldn't bind to a value doesn't filter.
    true
}

#[derive(Debug, Clone, Serialize)]
pub struct Computed {
    pub op: String,
    pub value: Value,
    /// Verbatim lines of the rows that produced the value.
    pub evidence: Vec<String>,
}

/// count / sum / min / max / argmax / argmin / list, optionally grouped.
pub fn compute(op: &str, rows: &[&Row], of: Option<&str>, by: Option<&str>) -> Computed {
    let ev = |rs: &[&Row]| rs.iter().map(|r| truncate(&r.line, 200)).collect::<Vec<_>>();
    let num = |r: &Row| of.and_then(|c| r.get(c)).and_then(number);
    // Comparisons normalize units ("512 MB" < "16 GB"); sums use the bare number.
    let qty = |r: &Row| of.and_then(|c| r.get(c)).and_then(quantity);
    let op_l = op.to_lowercase();
    if let Some(by) = by {
        // Group, then aggregate per group (count or sum), then pick for argmax/argmin.
        let mut groups: Vec<(String, Vec<&Row>)> = Vec::new();
        for r in rows {
            let k = r.get(by).unwrap_or("").to_string();
            match groups.iter_mut().find(|(g, _)| *g == k) {
                Some((_, v)) => v.push(r),
                None => groups.push((k, vec![r])),
            }
        }
        let agg = |rs: &[&Row]| -> f64 {
            if op_l.contains("sum") || (of.is_some() && !op_l.contains("count")) {
                rs.iter().filter_map(|r| num(r)).sum()
            } else {
                rs.len() as f64
            }
        };
        let mut scored: Vec<(String, f64, Vec<&Row>)> = groups.into_iter().map(|(g, rs)| (g, agg(&rs), rs)).collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        if op_l.contains("min") {
            scored.reverse();
        }
        let table: Vec<Value> = scored.iter().map(|(g, v, _)| json!([g, v])).collect();
        let best = scored.first();
        return Computed {
            op: op.to_string(),
            value: json!({"top": best.map(|b| b.0.clone()), "value": best.map(|b| b.1), "groups": table}),
            evidence: best.map(|b| ev(&b.2)).unwrap_or_default(),
        };
    }
    let value = match op_l.as_str() {
        o if o.contains("count") => json!(rows.len()),
        o if o.contains("sum") || o.contains("total") => {
            let s: f64 = rows.iter().filter_map(|r| num(r)).sum();
            // Money stays money: "$585.10", not 585.1.
            let cur = rows.iter().filter_map(|r| of.and_then(|c| r.get(c))).find_map(|v| v.chars().find(|c| "$€£¥₹".contains(*c)));
            let cents = rows.iter().filter_map(|r| of.and_then(|c| r.get(c))).any(|v| v.rsplit('.').next().is_some_and(|d| d.len() == 2 && d.chars().all(|c| c.is_ascii_digit())));
            match (cur, cents) {
                (Some(c), _) => json!(format!("{c}{}", with_commas(s))),
                (None, true) => json!(format!("{s:.2}")),
                _ => json!((s * 100.0).round() / 100.0),
            }
        }
        o if o.contains("argmax") || o.contains("max") || o.contains("most") || o.contains("highest") => {
            let best = rows.iter().max_by(|a, b| qty(a).unwrap_or(f64::MIN).total_cmp(&qty(b).unwrap_or(f64::MIN)));
            return Computed { op: op.into(), value: json!(best.map(|r| r.line.clone())), evidence: best.map(|r| vec![r.line.clone()]).unwrap_or_default() };
        }
        o if o.contains("argmin") || o.contains("min") || o.contains("least") || o.contains("lowest") => {
            let best = rows.iter().min_by(|a, b| qty(a).unwrap_or(f64::MAX).total_cmp(&qty(b).unwrap_or(f64::MAX)));
            return Computed { op: op.into(), value: json!(best.map(|r| r.line.clone())), evidence: best.map(|r| vec![r.line.clone()]).unwrap_or_default() };
        }
        _ => json!(rows.len()),
    };
    Computed { op: op.to_string(), value, evidence: ev(rows) }
}

/// 1290.5 → "1,290.50"
fn with_commas(x: f64) -> String {
    let s = format!("{x:.2}");
    let (int, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let neg = int.starts_with('-');
    let digits: Vec<char> = int.trim_start_matches('-').chars().collect();
    let mut out = String::new();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*c);
    }
    format!("{}{out}.{frac}", if neg { "-" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(number("$1,290.00"), Some(1290.0));
        assert_eq!(number("42.50 USD"), Some(42.5));
        assert_eq!(number("1290,5"), Some(1290.5));
        assert_eq!(number("12,000"), Some(12000.0));
        assert_eq!(number("n/a"), None);
        assert_eq!(number("16 GB LPDDR5X"), Some(16.0));
        assert_eq!(number("32 GB DDR5-5600"), Some(32.0));
        assert_eq!(number("8 GB DDR5 (1 free SO-DIMM slot, max 64 GB)"), Some(8.0));
        assert_eq!(number("1 290,5"), Some(1290.5));
        assert_eq!(number("1.290,50 €"), Some(1290.5));
        assert_eq!(number("-$25.00"), Some(-25.0));
        assert_eq!(number("$1,479.00"), Some(1479.0));
        assert_eq!(number("Page 2 of 18"), Some(2.0));
        assert!(quantity("512 MB").unwrap() < quantity("16 GB").unwrap());
        assert!(quantity("1 TB").unwrap() > quantity("512 GB").unwrap());
        assert!(quantity("90 min").unwrap() < quantity("2 h").unwrap());
    }

    #[test]
    fn conditions() {
        assert_eq!(numeric_cond("costs under $1,500"), Some((Cmp::Lt, 1500.0)));
        assert_eq!(numeric_cond("at least 16 GB"), Some((Cmp::Ge, 16.0)));
        assert_eq!(numeric_cond("status is Refunded"), None);
        assert_eq!(conjuncts("status is Refunded and customer is Linnea Dahlqvist").len(), 2);
    }

    #[test]
    fn sums_and_groups() {
        let r = |c: &str, s: &str, t: &str| Row {
            cells: vec![("Customer".into(), c.into()), ("Status".into(), s.into()), ("Total".into(), t.into())],
            line: format!("{c} {s} {t}"),
        };
        let rows = [r("A", "Refunded", "$10.50"), r("B", "Refunded", "$1,000.25"), r("A", "Paid", "$5")];
        let refs: Vec<&Row> = rows.iter().collect();
        let f = Filter { cond: "refunded".into(), col: Some("Status".into()), eq: Some("Refunded".into()), num: None, accept: None };
        let m: Vec<&Row> = refs.iter().copied().filter(|x| matches(x, &f)).collect();
        assert_eq!(compute("sum", &m, Some("Total"), None).value, json!("$1,010.75"));
        let g = compute("argmax count", &refs, None, Some("Customer"));
        assert_eq!(g.value["top"], json!("A"));
    }
}
