//! fab scripts: a small program holding only the logic of a request that is
//! known in advance (values carried between steps, loops, conditions,
//! fallbacks). Each leaf is decided on the live page at run time:
//! `do "<sub-goal>"` (fab works out the pages and clicks itself),
//! `read "<question>" -> x`, `test "<yes/no question>"`. The syntax is the
//! one cheap models wrote most correctly among flat labels, end-delimited
//! blocks and a Python subset (measured with ten models; see bench/LOG.md):
//!
//! ```text
//! set n = 1
//! while n <= 30
//!   do "open the comments of story $n on news.ycombinator.com"
//!   if test "the page shows a comment"
//!     read "the author of the first comment" -> author
//!     return author
//!   end
//!   set n = n + 1
//! end
//! fail "no story has comments"
//! ```
//!
//! Parsing accepts the variants models write unprompted (`x = …`, `-> $x`,
//! `{x}`, trailing colons, `elif`, `endif`, list markers). The grammar is
//! declared in `script.pest` (a PEG; pest generates the parser). Blocks are
//! lowered to jumps, so running a program is a flat loop over [`Op`]s.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// The in-page scraping runtime (`window.__fs`).
pub const SCRAPE_JS: &str = include_str!("js/scrape.js");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Leaf {
    Do,
    Read,
    Test,
}

/// Expressions that act on the page when evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Eff {
    /// `test "…"`: one yes/no check of the page.
    Test,
    /// `next page`: shows the next page or batch; false at the end.
    Next,
    /// `items "…"`: the not-yet-seen items of that list on the page.
    Items,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinOp {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Range,
}
impl BinOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Or => "or",
            Self::And => "and",
            Self::Equal => "==",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessEqual => "<=",
            Self::Greater => ">",
            Self::GreaterEqual => ">=",
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Remainder => "%",
            Self::Range => "range",
        }
    }
}
impl std::str::FromStr for BinOp {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "or" => Ok(Self::Or),
            "and" => Ok(Self::And),
            "==" => Ok(Self::Equal),
            "!=" => Ok(Self::NotEqual),
            "<" => Ok(Self::Less),
            "<=" => Ok(Self::LessEqual),
            ">" => Ok(Self::Greater),
            ">=" => Ok(Self::GreaterEqual),
            "+" => Ok(Self::Add),
            "-" => Ok(Self::Subtract),
            "*" => Ok(Self::Multiply),
            "/" => Ok(Self::Divide),
            "%" => Ok(Self::Remainder),
            "range" => Ok(Self::Range),
            _ => Err(format!("unknown binary operator {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Builtin {
    Contains,
    StartsWith,
    Lower,
    Upper,
    Text,
    Number,
    Length,
    Empty,
}
impl Builtin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::StartsWith => "starts_with",
            Self::Lower => "lower",
            Self::Upper => "upper",
            Self::Text => "trim",
            Self::Number => "number",
            Self::Length => "len",
            Self::Empty => "empty",
        }
    }
}
impl std::str::FromStr for Builtin {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "contains" | "includes" | "has" => Ok(Self::Contains),
            "starts_with" | "startswith" => Ok(Self::StartsWith),
            "lower" => Ok(Self::Lower),
            "upper" => Ok(Self::Upper),
            "trim" | "str" | "text" => Ok(Self::Text),
            "number" | "num" | "int" | "float" => Ok(Self::Number),
            "len" | "count" => Ok(Self::Length),
            "empty" | "is_empty" => Ok(Self::Empty),
            _ => Err(format!("unknown function {value}()")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Num(#[serde(with = "float_bits")] f64),
    Bool(bool),
    Str(String),
    Null,
    Var(String),
    List(Vec<Expr>),
    Effect(Eff, String),
    Call(Builtin, Vec<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
}

/// One instruction. Jumps are indices into the program.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    Leaf {
        kind: Leaf,
        text: String,
        save: Option<String>,
    },
    Set {
        var: String,
        expr: Expr,
    },
    /// Jump to `to` when `cond` is false. A `cond` with a `test` in it asks the page.
    JumpUnless {
        cond: Expr,
        to: usize,
    },
    Jump {
        to: usize,
    },
    /// Starts a `for`: evaluates the list into iterator slot `slot`.
    ForInit {
        slot: usize,
        list: Expr,
    },
    /// Next item into `var`, or jump to `exit` when done.
    ForNext {
        slot: usize,
        var: String,
        exit: usize,
    },
    Fail(String),
    Return(Vec<Expr>),
    /// `extract "a, b" [from item] -> x`: fields of an item, or of the page.
    Extract {
        fields: Vec<String>,
        from: Option<String>,
        save: String,
    },
    /// `open item ["which link"]`: follows the item's link. `leaf`: nothing
    /// before the matching `back` navigates, so the page can be read without
    /// leaving the list.
    Open {
        item: String,
        how: Option<String>,
        leaf: bool,
    },
    Back,
    /// `emit a, key: b`: one output record (objects merged; `key:` names a value).
    Emit(Vec<(Option<String>, Expr)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    ops: Vec<Op>,
    lines: Vec<usize>,
    slots: usize,
}

/// An instruction together with its one-based source location.
#[derive(Debug, Clone, Copy)]
pub struct LocatedInstruction<'a> {
    pub op: &'a Op,
    pub line: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgramDto {
    ops: Vec<Op>,
    lines: Vec<usize>,
    slots: usize,
}

impl Program {
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }
    pub fn line(&self, pc: usize) -> Option<usize> {
        self.lines.get(pc).copied()
    }
    pub fn slots(&self) -> usize {
        self.slots
    }
    pub fn instructions(&self) -> impl ExactSizeIterator<Item = LocatedInstruction<'_>> {
        self.ops
            .iter()
            .zip(&self.lines)
            .map(|(op, line)| LocatedInstruction { op, line: *line })
    }
}

impl TryFrom<ProgramDto> for Program {
    type Error = ParseError;
    fn try_from(dto: ProgramDto) -> Result<Self, Self::Error> {
        let invalid = |line, msg: &str| ParseError {
            line,
            msg: msg.into(),
        };
        if dto.ops.is_empty() {
            return Err(invalid(1, "the program has no steps"));
        }
        if dto.ops.len() != dto.lines.len() {
            return Err(invalid(1, "instruction and source location counts differ"));
        }
        if dto.slots > dto.ops.len() {
            return Err(invalid(1, "iterator count exceeds instruction count"));
        }
        for (op, line) in dto.ops.iter().zip(&dto.lines) {
            if *line == 0 {
                return Err(invalid(1, "source locations must be one-based"));
            }
            match op {
                Op::Jump { to } | Op::JumpUnless { to, .. } if *to > dto.ops.len() => {
                    return Err(invalid(*line, "jump is outside the program"));
                }
                Op::ForInit { slot, .. } | Op::ForNext { slot, .. } if *slot >= dto.slots => {
                    return Err(invalid(*line, "iterator slot is outside the program"));
                }
                Op::ForNext { exit, .. } if *exit > dto.ops.len() => {
                    return Err(invalid(*line, "iterator exit is outside the program"));
                }
                _ => {}
            }
        }
        validate_iterators(&dto)?;
        Ok(Self {
            ops: dto.ops,
            lines: dto.lines,
            slots: dto.slots,
        })
    }
}
/// Every reachable iterator access must be preceded by initialization on every path.
fn validate_iterators(dto: &ProgramDto) -> Result<(), ParseError> {
    use std::collections::{HashSet, VecDeque};
    let mut initializers = vec![None; dto.slots];
    for (pc, op) in dto.ops.iter().enumerate() {
        if let Op::ForInit { slot, .. } = op {
            if initializers[*slot].replace(pc).is_some() {
                return Err(ParseError {
                    line: dto.lines[pc],
                    msg: "iterator slot has multiple initializers".into(),
                });
            }
        }
    }
    if initializers.iter().any(Option::is_none) {
        return Err(ParseError {
            line: 1,
            msg: "iterator slot has no initializer".into(),
        });
    }
    let mut before: Vec<Option<HashSet<usize>>> = vec![None; dto.ops.len()];
    before[0] = Some(HashSet::new());
    let mut pending = VecDeque::from([0]);
    while let Some(pc) = pending.pop_front() {
        let mut initialized = before[pc]
            .as_ref()
            .expect("queued reachable instruction")
            .clone();
        if let Op::ForInit { slot, .. } = &dto.ops[pc] {
            initialized.insert(*slot);
        }
        let successors = match &dto.ops[pc] {
            Op::Jump { to } => vec![*to],
            Op::JumpUnless { to, .. } => vec![pc + 1, *to],
            Op::ForNext { exit, .. } => vec![pc + 1, *exit],
            Op::Return(_) | Op::Fail(_) => vec![],
            _ => vec![pc + 1],
        };
        for next in successors.into_iter().filter(|next| *next < dto.ops.len()) {
            let changed = match &mut before[next] {
                None => {
                    before[next] = Some(initialized.clone());
                    true
                }
                Some(previous) => {
                    let size = previous.len();
                    previous.retain(|slot| initialized.contains(slot));
                    previous.len() != size
                }
            };
            if changed {
                pending.push_back(next);
            }
        }
    }
    for (pc, op) in dto.ops.iter().enumerate() {
        if let Op::ForNext { slot, .. } = op {
            if initializers[*slot].is_none_or(|init| init >= pc)
                || before[pc].as_ref().is_some_and(|set| !set.contains(slot))
            {
                return Err(ParseError {
                    line: dto.lines[pc],
                    msg: "iterator may be used before initialization".into(),
                });
            }
        }
    }
    Ok(())
}

impl Serialize for Program {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ProgramDto {
            ops: self.ops.clone(),
            lines: self.lines.clone(),
            slots: self.slots,
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Program {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(ProgramDto::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EvalError {
    #[error("missing result for {kind:?} expression effect")]
    MissingEffectResult { kind: Eff },
    #[error("{0}")]
    InvalidValue(String),
}
impl From<String> for EvalError {
    fn from(value: String) -> Self {
        Self::InvalidValue(value)
    }
}
impl From<&str> for EvalError {
    fn from(value: &str) -> Self {
        Self::InvalidValue(value.into())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for ParseError {}

enum Block {
    /// Jump-unless ops to patch to the next branch, jumps to patch to the end.
    If {
        next: Option<usize>,
        ends: Vec<usize>,
        line: usize,
    },
    While {
        head: usize,
        exit: usize,
        breaks: Vec<usize>,
        line: usize,
    },
    For {
        head: usize,
        breaks: Vec<usize>,
        line: usize,
    },
}

struct P {
    ops: Vec<Op>,
    lines: Vec<usize>,
    blocks: Vec<Block>,
    defined: Vec<String>,
    slots: usize,
}

impl P {
    fn push(&mut self, op: Op, line: usize) -> usize {
        self.ops.push(op);
        self.lines.push(line);
        self.ops.len() - 1
    }

    fn patch(&mut self, at: usize, to: usize) {
        match &mut self.ops[at] {
            Op::JumpUnless { to: t, .. } | Op::Jump { to: t } | Op::ForNext { exit: t, .. } => {
                *t = to
            }
            _ => {}
        }
    }

    fn check_vars(&self, e: &Expr, line: usize) -> Result<(), ParseError> {
        let mut vars = vec![];
        e.vars(&mut vars);
        for v in vars {
            if !self.defined.contains(&v) {
                return Err(ParseError {
                    line,
                    msg: format!("{v} is used before it is set"),
                });
            }
        }
        Ok(())
    }

    fn check_text(&self, t: &str, line: usize) -> Result<(), ParseError> {
        for v in refs(t) {
            if !self.defined.contains(&v) {
                return Err(ParseError {
                    line,
                    msg: format!("${v} is used before it is set"),
                });
            }
        }
        Ok(())
    }

    fn define(&mut self, v: &str) {
        let v = v.split('.').next().unwrap_or(v);
        if !self.defined.iter().any(|d| d == v) {
            self.defined.push(v.to_string());
        }
    }
}

/// Parses and lowers a program.
/// The deepest nesting a line may have: brackets, and runs of unary `-` or
/// `not`. The generated parser recurses per level, so deeper input would
/// exhaust the stack instead of failing as a parse error.
const MAX_DEPTH: usize = 64;

/// Whether a line nests deeper than [`MAX_DEPTH`] (quoted text excluded).
fn too_deep(line: &str) -> bool {
    let (mut depth, mut unary, mut quoted) = (0usize, 0usize, false);
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '(' | '[' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return true;
                }
            }
            ')' | ']' => depth = depth.saturating_sub(1),
            '-' => {
                unary += 1;
                // A dash rule ("-----") is common; only a very long run recurses deep.
                if unary > 4 * MAX_DEPTH {
                    return true;
                }
            }
            c if c.is_whitespace() => {}
            _ => unary = 0,
        }
    }
    // `not not not …`
    let mut nots = 0;
    for w in line.split_whitespace() {
        nots = if w.eq_ignore_ascii_case("not") { nots + 1 } else { 0 };
        if nots > 4 * MAX_DEPTH {
            return true;
        }
    }
    false
}

pub fn parse(src: &str) -> Result<Program, ParseError> {
    let mut p = P {
        ops: vec![],
        lines: vec![],
        blocks: vec![],
        defined: vec![],
        slots: 0,
    };
    let err = |line: usize, msg: &str| ParseError {
        line,
        msg: msg.to_string(),
    };
    for (i, raw) in src.lines().enumerate() {
        let n = i + 1;
        if too_deep(raw) {
            return Err(err(n, &format!("nested more than {MAX_DEPTH} levels deep")));
        }
        let (l, kw, rest) = statement(raw);
        if l.is_empty() {
            continue;
        }
        let kw = kw.as_str();
        match kw {
            "do" | "read" | "test" => {
                let kind = match kw {
                    "do" => Leaf::Do,
                    "read" => Leaf::Read,
                    _ => Leaf::Test,
                };
                let (text, save) = leaf_args(rest);
                if text.is_empty() {
                    return Err(err(n, &format!("{kw} needs a quoted instruction")));
                }
                p.check_text(&text, n)?;
                if let Some(s) = &save {
                    p.define(s);
                }
                p.push(Op::Leaf { kind, text, save }, n);
            }
            "set" | "let" | "local" => {
                let (var, e) =
                    assignment(rest).ok_or_else(|| err(n, "expected: set name = value"))?;
                let e = expr(e).map_err(|m| err(n, &m))?;
                p.check_vars(&e, n)?;
                p.define(&var);
                p.push(Op::Set { var, expr: e }, n);
            }
            "if" => {
                let c = cond(rest).map_err(|m| err(n, &m))?;
                p.check_vars(&c, n)?;
                let at = p.push(Op::JumpUnless { cond: c, to: 0 }, n);
                p.blocks.push(Block::If {
                    next: Some(at),
                    ends: vec![],
                    line: n,
                });
            }
            "elif" | "else" | "elseif" => {
                let cond_src = match kw {
                    "else" => else_if(rest),
                    _ => Some(rest),
                };
                let Some(Block::If { next, .. }) = p.blocks.last() else {
                    return Err(err(n, "else without if"));
                };
                let next = *next;
                let end_jump = p.push(Op::Jump { to: 0 }, n);
                let here = p.ops.len();
                if let Some(at) = next {
                    p.patch(at, here);
                }
                let new_next = match cond_src.filter(|s| !s.is_empty()) {
                    Some(c) => {
                        let c = cond(c).map_err(|m| err(n, &m))?;
                        p.check_vars(&c, n)?;
                        Some(p.push(Op::JumpUnless { cond: c, to: 0 }, n))
                    }
                    None => None,
                };
                if let Some(Block::If { next, ends, .. }) = p.blocks.last_mut() {
                    ends.push(end_jump);
                    *next = new_next;
                }
            }
            "while" => {
                let c = cond(rest).map_err(|m| err(n, &m))?;
                p.check_vars(&c, n)?;
                let head = p.ops.len();
                let exit = p.push(Op::JumpUnless { cond: c, to: 0 }, n);
                p.blocks.push(Block::While {
                    head,
                    exit,
                    breaks: vec![],
                    line: n,
                });
            }
            "for" | "repeat" => {
                let (var, list, filter) = if kw == "repeat" {
                    // repeat N: a counted loop.
                    let k = repeat_count(rest);
                    (
                        "_i".to_string(),
                        Expr::Bin(
                            BinOp::Range,
                            Box::new(Expr::Num(1.0)),
                            Box::new(expr(k).map_err(|m| err(n, &m))?),
                        ),
                        None,
                    )
                } else {
                    let (v, l, f) =
                        for_args(rest).ok_or_else(|| err(n, "expected: for x in <list>"))?;
                    (
                        v.to_string(),
                        list_expr(l).map_err(|m| err(n, &m))?,
                        f,
                    )
                };
                p.check_vars(&list, n)?;
                let slot = p.slots;
                p.slots += 1;
                p.push(Op::ForInit { slot, list }, n);
                p.define(&var);
                let head = p.push(Op::ForNext { slot, var, exit: 0 }, n);
                p.blocks.push(Block::For {
                    head,
                    breaks: vec![],
                    line: n,
                });
                // `for x in <list> where <condition>`: an item the condition
                // rejects is left, as if the loop body had continued.
                if let Some(src) = filter {
                    if src.trim().is_empty() {
                        return Err(err(n, "a where condition is needed, e.g. for c in items \"comments\" where c.pinned == null"));
                    }
                    let c = cond(src).map_err(|m| err(n, &format!("in the where condition: {m}")))?;
                    p.check_vars(&c, n)?;
                    p.push(Op::JumpUnless { cond: Expr::Not(Box::new(c)), to: head }, n);
                }
            }
            "end" | "endif" | "endwhile" | "endfor" | "done" | "}" => {
                let Some(b) = p.blocks.pop() else {
                    return Err(err(n, "end without a block"));
                };
                match b {
                    Block::If { next, ends, .. } => {
                        let here = p.ops.len();
                        if let Some(at) = next {
                            p.patch(at, here);
                        }
                        for e in ends {
                            p.patch(e, here);
                        }
                    }
                    Block::While {
                        head, exit, breaks, ..
                    } => {
                        p.push(Op::Jump { to: head }, n);
                        let here = p.ops.len();
                        p.patch(exit, here);
                        for b in breaks {
                            p.patch(b, here);
                        }
                    }
                    Block::For { head, breaks, .. } => {
                        p.push(Op::Jump { to: head }, n);
                        let here = p.ops.len();
                        p.patch(head, here);
                        for b in breaks {
                            p.patch(b, here);
                        }
                    }
                }
            }
            "break" | "continue" => {
                let at = p.push(Op::Jump { to: 0 }, n);
                let target = p
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|b| !matches!(b, Block::If { .. }));
                match (kw, target) {
                    ("break", Some(Block::While { breaks, .. } | Block::For { breaks, .. })) => {
                        breaks.push(at)
                    }
                    ("continue", Some(Block::While { head, .. } | Block::For { head, .. })) => {
                        let h = *head;
                        p.patch(at, h);
                    }
                    _ => return Err(err(n, &format!("{kw} outside a loop"))),
                }
            }
            "stop" => {
                p.push(Op::Return(vec![]), n);
            }
            "extract" | "get" | "scrape" => {
                let (fields, from, save) = extract_args(rest).map_err(|m| err(n, &m))?;
                if let Some(f) = &from {
                    p.check_vars(&Expr::Var(f.clone()), n)?;
                }
                p.define(&save);
                p.push(Op::Extract { fields, from, save }, n);
            }
            "open" | "follow" | "visit" => {
                let (item, how) = open_args(rest);
                if item.is_empty() || item.starts_with('"') {
                    return Err(err(
                        n,
                        "expected: open <item> [\"which link\"] (to go to a site, use do)",
                    ));
                }
                p.check_vars(&Expr::Var(item.clone()), n)?;
                p.push(
                    Op::Open {
                        item,
                        how,
                        leaf: false,
                    },
                    n,
                );
            }
            "back" => {
                p.push(Op::Back, n);
            }
            "emit" | "yield" | "output" | "print" => {
                let mut es = vec![];
                for part in split_top(emit_inner(rest), Rule::comma_parts) {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    // `key: value` (a key may be quoted).
                    let (key, e) = match split_top(part, Rule::colon_parts).as_slice() {
                        [k, v] if !k.trim().is_empty() && !k.contains('(') => {
                            (Some(field_name(&unquote(k))), v.trim())
                        }
                        _ => (None, part),
                    };
                    let e = expr(e).map_err(|m| err(n, &m))?;
                    p.check_vars(&e, n)?;
                    es.push((key, e));
                }
                p.push(Op::Emit(es), n);
            }
            "fail" => {
                let t = unquote(rest);
                p.check_text(&t, n)?;
                p.push(Op::Fail(t), n);
            }
            "return" => {
                let mut es = vec![];
                for part in split_top(rest, Rule::comma_parts) {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    let e = expr(part).map_err(|m| err(n, &m))?;
                    p.check_vars(&e, n)?;
                    es.push(e);
                }
                p.push(Op::Return(es), n);
            }
            _ => {
                // `x = …` / `$x = …` without `set`; `x = read "…"` / `x = do "…"`.
                if let Some((var, e)) = assignment(l) {
                    let rhs = assign_rhs(e);
                    if let Rhs::Extract(body) = rhs {
                        let (fields, from) = save_name(&var)
                            .and_then(|_| extract_body(body))
                            .map_err(|m| err(n, &m))?;
                        if let Some(f) = &from {
                            p.check_vars(&Expr::Var(f.clone()), n)?;
                        }
                        p.define(&var);
                        p.push(
                            Op::Extract {
                                fields,
                                from,
                                save: var,
                            },
                            n,
                        );
                        continue;
                    }
                    if let Rhs::Leaf(kind, text) = rhs {
                        let text = unquote(text);
                        p.check_text(&text, n)?;
                        p.define(&var);
                        p.push(
                            Op::Leaf {
                                kind,
                                text,
                                save: Some(var),
                            },
                            n,
                        );
                        continue;
                    }
                    let e = expr(e).map_err(|m| err(n, &m))?;
                    p.check_vars(&e, n)?;
                    p.define(&var);
                    p.push(Op::Set { var, expr: e }, n);
                    continue;
                }
                return Err(err(
                    n,
                    &format!(
                        "unknown statement \"{kw}\" (steps are do, read and test; logic is set, if/else/end, while/end, for/end, break, fail, return)"
                    ),
                ));
            }
        }
    }
    if let Some(b) = p.blocks.last() {
        let line = match b {
            Block::If { line, .. } | Block::While { line, .. } | Block::For { line, .. } => *line,
        };
        return Err(err(line, "this block has no end"));
    }
    if p.ops.is_empty() {
        return Err(err(1, "the program has no steps"));
    }
    let mut ops = p.ops;
    for i in 0..ops.len() {
        if let Op::Open { .. } = &ops[i] {
            let mut leaf = false;
            for o in &ops[i + 1..] {
                match o {
                    Op::Back => {
                        leaf = true;
                        break;
                    }
                    // What needs the live page: a `do`, or opening again.
                    // Reading fields, lists and questions works on the fetched
                    // page; paging opens it for real when it happens.
                    Op::Open { .. } | Op::Leaf { kind: Leaf::Do, .. } => break,
                    _ => {}
                }
            }
            if let Op::Open { leaf: l, .. } = &mut ops[i] {
                *l = leaf;
            }
        }
    }
    Program::try_from(ProgramDto {
        ops,
        lines: p.lines,
        slots: p.slots,
    })
}

/// Likely mistakes in a program that parses: advice for the writer.
pub fn lint(p: &Program) -> Vec<String> {
    let mut out = vec![];
    let loops: Vec<(usize, usize)> = p
        .ops
        .iter()
        .enumerate()
        .filter_map(|(i, o)| {
            if let Op::Jump { to } = o {
                (*to <= i).then_some((*to, i))
            } else {
                None
            }
        })
        .collect();
    let effects = |o: &Op| -> Vec<Eff> {
        match o {
            Op::JumpUnless { cond: e, .. }
            | Op::Set { expr: e, .. }
            | Op::ForInit { list: e, .. } => e.effects().into_iter().map(|(k, _)| k).collect(),
            _ => vec![],
        }
    };
    let mut has_next = false;
    let mut has_items = false;
    for (i, o) in p.ops.iter().enumerate() {
        let ef = effects(o);
        has_items |= ef.contains(&Eff::Items);
        if ef.contains(&Eff::Next) {
            has_next = true;
            if !loops.iter().any(|(a, b)| *a <= i && i <= *b) {
                out.push(format!("line {}: `next page` outside a loop shows only one more page; loop: while true … if not next page … break … end … end", p.lines[i]));
            }
        }
    }
    if has_items && !has_next {
        out.push("the program reads only the first page of items; unless the user asked for this page only, loop over the pages with next page (a limit like \"the first 20\" still needs it: the first page may have fewer)".into());
    }
    out
}

/// A field's key: "Employment type" → employment_type.
pub fn field_name(f: &str) -> String {
    let mut out = String::new();
    for c in f.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

/// `$name` and `{name}` references in a leaf's text.
pub fn refs(t: &str) -> Vec<String> {
    let mut out = vec![];
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' && i + 1 < b.len() && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_') {
            let s = i + 1;
            let mut e = s;
            while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_') {
                e += 1;
            }
            out.push(t[s..e].to_string());
            i = e;
        } else {
            i += 1;
        }
    }
    out
}

/// Inserts values for `$name` (longest names first) and `{name}` (when `name`
/// is set); `{{…}}` is left alone.
pub fn interpolate(t: &str, vars: &HashMap<String, Value>) -> String {
    let mut names: Vec<&String> = vars.keys().collect();
    names.sort_by_key(|k| std::cmp::Reverse(k.len()));
    let mut out = t.to_string();
    for k in names {
        let v = vars[k].to_string();
        out = out.replace(&format!("${k}"), &v);
        // `{k}`, but not inside a `{{secret}}`.
        let brace = format!("{{{k}}}");
        let mut s = String::new();
        let mut rest = out.as_str();
        while let Some(i) = rest.find(&brace) {
            let (before, after) = (&rest[..i], &rest[i + brace.len()..]);
            s.push_str(before);
            s.push_str(if before.ends_with('{') || after.starts_with('}') {
                &brace
            } else {
                &v
            });
            rest = after;
        }
        s.push_str(rest);
        out = s;
    }
    out
}

// ---- parsing (the grammar is script.pest) ----

#[derive(pest_derive::Parser)]
#[grammar = "script.pest"]
struct Grammar;

type Node<'a> = pest::iterators::Pair<'a, Rule>;

/// `text` read as `rule`, when it matches.
fn read(rule: Rule, text: &str) -> Option<Node<'_>> {
    use pest::Parser;
    Grammar::parse(rule, text).ok()?.next()
}

/// The parts of a node (without the end-of-input marker).
fn parts(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    node.into_inner().filter(|p| p.as_rule() != Rule::EOI)
}

/// The first part of `node` read as `rule`.
fn part(node: Node<'_>, rule: Rule) -> Option<Node<'_>> {
    parts(node).find(|p| p.as_rule() == rule)
}

/// The statement on a line (without list markers, comments, code fences and
/// a trailing `:`, ` {` or ` then`), its keyword (lowercased) and the text
/// after it; for a line without a keyword, its first word and the whole line.
fn statement(raw: &str) -> (&str, String, &str) {
    let Some(body) = read(Rule::line, raw).and_then(|line| part(line, Rule::body)) else {
        return ("", String::new(), "");
    };
    let l = body.as_str();
    let mut words = parts(body);
    match words.next() {
        Some(kw) if kw.as_rule() == Rule::keyword => (
            l,
            kw.as_str().to_ascii_lowercase(),
            words.next().map_or("", |rest| rest.as_str()),
        ),
        Some(word) => (l, word.as_str().to_lowercase(), l),
        None => ("", String::new(), ""),
    }
}

/// `"text" -> name` → ("text", Some(name)).
fn leaf_args(rest: &str) -> (String, Option<String>) {
    let Some(split) = read(Rule::leaf_args, rest).and_then(|a| parts(a).next()) else {
        return (unquote(rest), None);
    };
    if split.as_rule() == Rule::leaf_text {
        return (unquote(split.as_str()), None);
    }
    let mut pieces = parts(split);
    let text = pieces.next().map_or("", |t| t.as_str());
    let name = pieces
        .next()
        .and_then(|save| part(save, Rule::save_name))
        .map_or("", |n| n.as_str());
    (unquote(text), (!name.is_empty()).then(|| name.to_string()))
}

/// A text without the quotes around it (`\"` stands for a quote, `\n` and
/// `\t` for a line break and a tab).
fn unquote(s: &str) -> String {
    let Some(whole) = read(Rule::unquoted, s).and_then(|u| parts(u).next()) else {
        return s.trim().to_string();
    };
    match whole.as_rule() {
        Rule::raw_whole => whole.as_str().to_string(),
        _ => parts(whole)
            .map(|p| match p.as_rule() {
                Rule::esc_quote => "\"",
                Rule::esc_ctl if p.as_str() == "\\n" => "\n",
                Rule::esc_ctl => "\t",
                _ => p.as_str(),
            })
            .collect(),
    }
}

/// `n = n + 1` / `$n = 1` → ("n", "n + 1").
fn assignment(s: &str) -> Option<(String, &str)> {
    let mut pieces = parts(read(Rule::assignment, s)?);
    let target = pieces.next()?.as_str().to_string();
    Some((target, pieces.next().map_or("", |v| v.as_str())))
}

/// The value of an assignment without `set`.
#[derive(Clone, Copy)]
enum Rhs<'a> {
    /// `x = extract "…" [from item]`
    Extract(&'a str),
    /// `x = read "…"`
    Leaf(Leaf, &'a str),
    Expr,
}

fn assign_rhs(e: &str) -> Rhs<'_> {
    let Some(rhs) = read(Rule::assign_rhs, e).and_then(|r| parts(r).next()) else {
        return Rhs::Expr;
    };
    let rule = rhs.as_rule();
    let mut pieces = parts(rhs);
    match rule {
        Rule::rhs_extract => Rhs::Extract(pieces.next().map_or("", |r| r.as_str())),
        Rule::rhs_leaf => {
            let kind = match pieces.next().map(|k| k.as_str().trim().to_ascii_lowercase()) {
                Some(k) if k == "do" => Leaf::Do,
                Some(k) if k == "read" => Leaf::Read,
                _ => Leaf::Test,
            };
            Rhs::Leaf(kind, pieces.next().map_or("", |r| r.as_str()))
        }
        _ => Rhs::Expr,
    }
}

/// The condition of `else if <cond>` (`else` ignores other text).
fn else_if(rest: &str) -> Option<&str> {
    read(Rule::else_args, rest)
        .and_then(|a| part(a, Rule::else_cond))
        .map(|c| c.as_str())
}

/// `x in <list> [where <condition>]` → ("x", "<list>", condition).
fn for_args(rest: &str) -> Option<(&str, &str, Option<&str>)> {
    let args = read(Rule::for_args, rest)?;
    let var = part(args.clone(), Rule::for_var)?.as_str();
    let list = part(args.clone(), Rule::for_list)?.as_str();
    let filter = part(args, Rule::for_where).map(|w| part(w, Rule::for_cond).map_or("", |c| c.as_str()));
    Some((var, list, filter))
}

/// `3 times` → "3".
fn repeat_count(rest: &str) -> &str {
    read(Rule::repeat_args, rest)
        .and_then(|a| part(a, Rule::repeat_count))
        .map_or(rest, |c| c.as_str())
}

/// `item "which link"` → (item, Some(which link)).
fn open_args(rest: &str) -> (String, Option<String>) {
    let Some(args) = read(Rule::open_args, rest) else {
        return (String::new(), None);
    };
    let item = part(args.clone(), Rule::open_item).map_or("", |i| i.as_str());
    let how = part(args, Rule::open_how)
        .map(|h| unquote(h.as_str()))
        .filter(|h| !h.is_empty());
    (item.to_string(), how)
}

/// `{a, key: b}` → "a, key: b".
fn emit_inner(rest: &str) -> &str {
    read(Rule::emit_args, rest)
        .and_then(|a| part(a, Rule::emit_inner))
        .map_or(rest, |i| i.as_str())
}

/// `"a, b and c" [from x] -> y` → ([a, b, c], Some(x), y).
fn extract_args(rest: &str) -> Result<(Vec<String>, Option<String>, String), String> {
    let mut pieces = read(Rule::extract_args, rest)
        .and_then(|a| parts(a).next())
        .map(parts)
        .ok_or("expected: extract \"fields\" [from item] -> name")?;
    let body = pieces.next().map_or("", |b| b.as_str());
    let save = save_of(pieces.next())?;
    let (fields, from) = extract_body(body.trim())?;
    Ok((fields, from, save))
}

/// The name after `extract … ->`.
fn save_name(s: &str) -> Result<String, String> {
    save_of(read(Rule::ex_save, s))
}

fn save_of(save: Option<Node<'_>>) -> Result<String, String> {
    save.and_then(|s| part(s, Rule::save_ok))
        .and_then(|ok| part(ok, Rule::save_ident))
        .map(|name| name.as_str().to_string())
        .ok_or_else(|| "extract needs -> name".into())
}

/// `"a, b and c" from x` → ([a, b, c], Some(x)).
fn extract_body(body: &str) -> Result<(Vec<String>, Option<String>), String> {
    let (fields, from) = match read(Rule::extract_body, body).and_then(|b| parts(b).next()) {
        Some(split) if split.as_rule() == Rule::ex_from => {
            let fields = part(split.clone(), Rule::ex_fields_from).map_or("", |f| f.as_str());
            let item = part(split, Rule::ex_item).map_or("", |i| i.as_str());
            (fields, Some(item.to_string()))
        }
        _ => (body, None),
    };
    let text = unquote(fields);
    let fields: Vec<String> = read(Rule::field_list, &text)
        .map(|list| {
            parts(list)
                .map(|f| field_name(f.as_str()))
                .filter(|f| !f.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if fields.is_empty() {
        return Err("extract needs fields, e.g. extract \"title, price\" from item -> row".into());
    }
    Ok((fields, from))
}

/// Splits at top-level separators (`rule`: `comma_parts` or `colon_parts`).
fn split_top(s: &str, rule: Rule) -> Vec<&str> {
    read(rule, s).map_or_else(|| vec![s], |list| parts(list).map(|p| p.as_str()).collect())
}

fn cond(s: &str) -> Result<Expr, String> {
    expr(s)
}

/// A `for` list: `range(a, b)`, `a..b`, `a, b, c`, or a value.
fn list_expr(s: &str) -> Result<Expr, String> {
    let Some(list) = read(Rule::list_expr, s).and_then(|l| parts(l).next()) else {
        return expr(s);
    };
    let range = |a, b| Ok(Expr::Bin(BinOp::Range, Box::new(a), Box::new(b)));
    match list.as_rule() {
        Rule::range_call => {
            let inner = part(list, Rule::range_inner).map_or("", |i| i.as_str());
            match split_top(inner, Rule::comma_parts).as_slice() {
                [b] => range(Expr::Num(1.0), expr(b)?),
                [a, b, ..] => range(
                    expr(a)?,
                    Expr::Bin(
                        BinOp::Subtract,
                        Box::new(expr(b)?),
                        Box::new(Expr::Num(1.0)),
                    ),
                ),
                [] => Err("range needs a bound".into()),
            }
        }
        Rule::range_dots => {
            let lo = part(list.clone(), Rule::range_lo).map_or("", |l| l.as_str());
            let hi = part(list, Rule::range_hi).map_or("", |h| h.as_str());
            range(expr(lo)?, expr(hi)?)
        }
        _ => {
            let inner = part(list, Rule::bracket_inner).map_or(s, |i| i.as_str());
            let items = split_top(inner, Rule::comma_parts);
            if items.len() > 1 {
                return Ok(Expr::List(
                    items.iter().map(|p| expr(p)).collect::<Result<_, _>>()?,
                ));
            }
            expr(s)
        }
    }
}

/// Parses an expression: `or`, `and`, `not`, comparisons, `+ - * /`,
/// numbers, strings, names, `test "…"`, `(…)`, `[…]`.
pub fn expr(s: &str) -> Result<Expr, String> {
    if too_deep(s) {
        return Err(format!("nested more than {MAX_DEPTH} levels deep"));
    }
    let parsed = parse_expr(s);
    if parsed.is_ok() {
        return parsed;
    }
    // A character that starts no token (or a malformed number) is reported
    // before any misplaced token.
    for lexeme in read(Rule::lexemes, s).into_iter().flat_map(parts) {
        match lexeme.as_rule() {
            Rule::bad_char => return Err(format!("unexpected \"{}\"", lexeme.as_str())),
            Rule::number => {
                number(lexeme.as_str())?;
            }
            _ => {}
        }
    }
    parsed
}

fn parse_expr(s: &str) -> Result<Expr, String> {
    let expression = read(Rule::expression, s).ok_or_else(|| format!("unexpected text in {s}"))?;
    let mut pieces = expression.into_inner();
    let e = walk(pieces.next().ok_or("expected a value")?)?;
    match pieces.next() {
        Some(end) if end.as_rule() == Rule::EOI => Ok(e),
        Some(trailing) => Err(match parts(trailing).next() {
            Some(token) => format!("unexpected \"{}\" in {s}", token_text(&token)),
            None => format!("unexpected text in {s}"),
        }),
        // Stopped at a character that starts no token (see `expr`).
        None => Err(format!("unexpected text in {s}")),
    }
}

/// `1,250.50` → 1250.5.
fn number(t: &str) -> Result<f64, String> {
    let t: String = t.chars().filter(|c| *c != ',').collect();
    t.parse().map_err(|_| format!("bad number {t}"))
}

/// The text of a string token, escapes applied: `\n` and `\t` are a line
/// break and a tab (text with paragraphs), any other `\x` is `x`.
fn string_value(s: &Node<'_>) -> String {
    s.clone()
        .into_inner()
        .flat_map(|quoted| quoted.into_inner())
        .map(|chunk| match chunk.as_rule() {
            Rule::str_esc => match &chunk.as_str()[1..] {
                "n" => "\n",
                "t" => "\t",
                c => c,
            },
            _ => chunk.as_str(),
        })
        .collect()
}

/// A name without its `$`.
fn name<'a>(ident: &Node<'a>) -> &'a str {
    let t = ident.as_str();
    t.strip_prefix('$').unwrap_or(t)
}

/// A token as error messages show it.
fn token_text(t: &Node<'_>) -> String {
    match t.as_rule() {
        Rule::number => number(t.as_str()).map_or_else(|m| m, |n| n.to_string()),
        Rule::string => format!("\"{}\"", string_value(t)),
        Rule::ident => name(t).to_string(),
        _ if t.as_str() == "=" => "==".into(),
        _ => t.as_str().to_string(),
    }
}

/// An expression node of the grammar → an [`Expr`], or the first error in it.
fn walk(node: Node<'_>) -> Result<Expr, String> {
    let rule = node.as_rule();
    let text = node.as_str();
    let mut pieces = parts(node.clone());
    let mut next = || pieces.next().ok_or_else(|| "expected a value".to_string());
    Ok(match rule {
        Rule::or_expr | Rule::and_expr | Rule::add_expr | Rule::mul_expr => {
            let mut l = walk(next()?)?;
            while let Ok(op) = next() {
                let op = match op.as_rule() {
                    Rule::or_op => BinOp::Or,
                    Rule::and_op => BinOp::And,
                    _ => op.as_str().parse()?,
                };
                l = Expr::Bin(op, Box::new(l), Box::new(walk(next()?)?));
            }
            l
        }
        Rule::not_expr => {
            let first = next()?;
            if first.as_rule() == Rule::not_op {
                Expr::Not(Box::new(walk(next()?)?))
            } else {
                walk(first)?
            }
        }
        Rule::cmp_expr => {
            let l = walk(next()?)?;
            let Ok(op) = next() else {
                return Ok(l);
            };
            let op = match op.as_rule() {
                Rule::kw_is => {
                    let mut r = next()?;
                    let neg = r.as_rule() == Rule::kw_not;
                    if neg {
                        r = next()?;
                    }
                    let op = if neg { BinOp::NotEqual } else { BinOp::Equal };
                    return Ok(Expr::Bin(op, Box::new(l), Box::new(walk(r)?)));
                }
                _ if op.as_str() == "=" => BinOp::Equal,
                _ => op.as_str().parse()?,
            };
            Expr::Bin(op, Box::new(l), Box::new(walk(next()?)?))
        }
        Rule::atom => walk(next()?)?,
        Rule::number => Expr::Num(number(text)?),
        Rule::string => Expr::Str(string_value(&node)),
        Rule::lit_null => Expr::Null,
        Rule::lit_true => Expr::Bool(true),
        Rule::lit_false => Expr::Bool(false),
        Rule::var => Expr::Var(name(&next()?).to_string()),
        Rule::effect => {
            let kw = next()?;
            let eff = match kw.as_rule() {
                Rule::kw_test => Eff::Test,
                Rule::kw_items => Eff::Items,
                _ => Eff::Next,
            };
            match next() {
                Ok(s) if s.as_rule() == Rule::string => Expr::Effect(eff, string_value(&s)),
                // `next page`, `next batch`
                Ok(what) => Expr::Effect(eff, what.into_inner().next().map_or("", |i| name(&i)).into()),
                Err(_) if eff == Eff::Next => Expr::Effect(eff, "page".into()),
                Err(_) => return Err(format!("{} needs a quoted description", name(&kw))),
            }
        }
        Rule::call => {
            let f = next()?;
            let args = items(next()?, Rule::call_args, Rule::close_paren, "missing )")?;
            Expr::Call(name(&f).to_lowercase().parse()?, args)
        }
        Rule::quoted_field => {
            let head = next()?;
            Expr::Var(format!("{}{}", name(&head), field_name(&string_value(&next()?))))
        }
        Rule::bracket_field => {
            let head = next()?;
            Expr::Var(format!("{}.{}", name(&head), field_name(&string_value(&next()?))))
        }
        Rule::spaced_field => {
            let mut v = name(&next()?).to_string();
            while let Ok(word) = next() {
                v.push('_');
                v.push_str(&name(&word).to_lowercase());
            }
            match v.split_once('.') {
                Some((head, field)) => Expr::Var(format!("{head}.{}", field_name(field))),
                None => Expr::Var(v),
            }
        }
        Rule::paren => {
            let e = walk(next()?)?;
            if next().is_err() {
                return Err("missing )".into());
            }
            e
        }
        Rule::list => Expr::List(items(next()?, Rule::list_items, Rule::close_bracket, "missing ]")?),
        Rule::neg => Expr::Bin(
            BinOp::Subtract,
            Box::new(Expr::Num(0.0)),
            Box::new(walk(next()?)?),
        ),
        Rule::bad_atom => {
            return Err(match next() {
                Ok(token) => format!("unexpected \"{}\"", token_text(&token)),
                Err(_) => "expected a value".into(),
            });
        }
        _ => return Err(format!("unexpected \"{text}\"")),
    })
}

/// The values of `f(a, b)` or `[a, b]`: `rule` nests the rest after a comma.
fn items(mut node: Node<'_>, rule: Rule, close: Rule, missing: &str) -> Result<Vec<Expr>, String> {
    let mut out = vec![];
    loop {
        let mut pieces = parts(node);
        match pieces.next() {
            Some(end) if end.as_rule() == close => return Ok(out),
            Some(value) => out.push(walk(value)?),
            None => return Err(missing.into()),
        }
        match pieces.next() {
            Some(more) if more.as_rule() == rule => node = more,
            Some(_) => return Ok(out),
            None => return Err(missing.into()),
        }
    }
}

impl Expr {
    fn vars(&self, out: &mut Vec<String>) {
        match self {
            Expr::Var(v) => out.push(v.split('.').next().unwrap_or(v).to_string()),
            Expr::Str(s) | Expr::Effect(_, s) => out.extend(refs(s)),
            Expr::Call(_, xs) => xs.iter().for_each(|x| x.vars(out)),
            Expr::Null | Expr::Bool(_) => {}
            Expr::List(xs) => xs.iter().for_each(|x| x.vars(out)),
            Expr::Not(x) => x.vars(out),
            Expr::Bin(_, a, b) => {
                a.vars(out);
                b.vars(out);
            }
            Expr::Num(_) => {}
        }
    }

    /// The page effects in this expression (`test`, `next`, `items`), in
    /// evaluation order; `eval` takes their results in the same order.
    pub fn effects(&self) -> Vec<(Eff, String)> {
        let mut out = vec![];
        self.collect_effects(&mut out);
        out
    }

    fn collect_effects(&self, out: &mut Vec<(Eff, String)>) {
        match self {
            Expr::Effect(e, q) => out.push((*e, q.clone())),
            Expr::List(xs) | Expr::Call(_, xs) => xs.iter().for_each(|x| x.collect_effects(out)),
            Expr::Not(x) => x.collect_effects(out),
            Expr::Bin(_, a, b) => {
                a.collect_effects(out);
                b.collect_effects(out);
            }
            _ => {}
        }
    }
}

// ---- values ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Str(String),
    Num(#[serde(with = "float_bits")] f64),
    Bool(bool),
    List(Vec<Value>),
    /// A record: fields in order.
    Obj(Vec<(String, Value)>),
    /// A handle to an item on the page (the runner's table).
    Item(u32),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => f.write_str(s),
            Value::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => write!(f, "{}", *n as i64),
            Value::Num(n) => write!(f, "{n}"),
            Value::Bool(b) => f.write_str(if *b { "yes" } else { "no" }),
            Value::List(xs) => f.write_str(
                &xs.iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Value::Null => f.write_str(""),
            Value::Obj(fs) => f.write_str(
                &fs.iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            Value::Item(i) => write!(f, "item {i}"),
        }
    }
}

impl Value {
    /// The first number in a value: "$1,234.50" → 1234.5, "900 kr each" → 900.
    pub fn num(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Str(s) => {
                let cs: Vec<char> = s.chars().collect();
                let start = cs.iter().position(char::is_ascii_digit)?;
                let neg = start > 0 && cs[start - 1] == '-';
                let mut t = String::new();
                for (k, c) in cs[start..].iter().enumerate() {
                    if c.is_ascii_digit()
                        || (*c == '.' && cs.get(start + k + 1).is_some_and(char::is_ascii_digit))
                    {
                        t.push(*c);
                    } else if *c == ',' && cs.get(start + k + 1).is_some_and(char::is_ascii_digit) {
                    } else {
                        break;
                    }
                }
                t.parse::<f64>().ok().map(|n| if neg { -n } else { n })
            }
            Value::List(_) | Value::Null | Value::Obj(_) | Value::Item(_) => None,
        }
    }

    pub fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0,
            Value::List(xs) => !xs.is_empty(),
            Value::Null => false,
            Value::Obj(fs) => {
                fs.iter().any(|(k, v)| !k.starts_with("__") && v.truthy())
                    || fs.iter().any(|(k, _)| k == "__item")
            }
            Value::Item(_) => true,
            Value::Str(s) => {
                let l = s.trim().to_lowercase();
                !(l.is_empty()
                    || ["no", "false", "none", "0", "n/a", "not found", "nothing"]
                        .contains(&l.as_str())
                    || l.starts_with("no ")
                    || l.starts_with("no,")
                    || l.starts_with("no."))
            }
        }
    }

    /// The values a `for` iterates: a list's items (item handles stay whole).
    pub fn items_or_handles(&self) -> Vec<Value> {
        match self {
            Value::Item(_) | Value::Obj(_) => vec![self.clone()],
            other => other.items(),
        }
    }

    /// Items of a list value; text from a read splits on lines, then `;`, then `,`.
    pub fn items(&self) -> Vec<Value> {
        match self {
            Value::List(xs) => xs.clone(),
            Value::Str(s) => {
                let sep = if s.contains('\n') {
                    "\n"
                } else if s.contains(';') {
                    ";"
                } else {
                    ","
                };
                s.split(sep)
                    .map(|x| x.trim().trim_start_matches(['-', '*', '•']).trim())
                    .filter(|x| !x.is_empty())
                    .map(|x| Value::Str(x.to_string()))
                    .collect()
            }
            other => vec![other.clone()],
        }
    }
}

impl Value {
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Str(s) => json!(s),
            Value::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => json!(*n as i64),
            Value::Num(n) => json!(n),
            Value::Bool(b) => json!(b),
            Value::List(xs) => serde_json::Value::Array(xs.iter().map(Value::to_json).collect()),
            Value::Obj(fs) => serde_json::Value::Object(
                fs.iter().map(|(k, v)| (k.clone(), v.to_json())).collect(),
            ),
            Value::Item(i) => json!(format!("item {i}")),
        }
    }

    pub fn from_json(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Num(n.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(s) => Value::Str(s.clone()),
            serde_json::Value::Array(xs) => Value::List(xs.iter().map(Value::from_json).collect()),
            serde_json::Value::Object(m) => Value::Obj(
                m.iter()
                    .map(|(k, v)| (k.clone(), Value::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// A record of one field stands for its value (`extract "price" -> p`
    /// used as a price).
    pub fn scalar(&self) -> Value {
        match self {
            Value::Obj(fs) => {
                let vis: Vec<&(String, Value)> =
                    fs.iter().filter(|(k, _)| !k.starts_with("__")).collect();
                if vis.len() == 1 && fs.len() == 1 {
                    vis[0].1.clone()
                } else {
                    self.clone()
                }
            }
            other => other.clone(),
        }
    }

    /// A field of a record (by exact key, then snake-cased key).
    pub fn get(&self, k: &str) -> Value {
        match self {
            Value::Obj(fs) => {
                let fk = field_name(k);
                if let Some((_, v)) = fs.iter().find(|(a, _)| a == k || field_name(a) == fk) {
                    return v.clone();
                }
                if matches!(fk.as_str(), "level" | "depth" | "indent") {
                    if let Some((_, v)) = fs.iter().find(|(a, _)| a == "__level") {
                        return v.clone();
                    }
                }
                // `comment.username` for the field "username of the first commenter".
                let vis: Vec<&(String, Value)> =
                    fs.iter().filter(|(a, _)| !a.starts_with("__")).collect();
                let hits: Vec<&&(String, Value)> = vis
                    .iter()
                    .filter(|(a, _)| {
                        field_name(a).split('_').any(|w| w == fk)
                            || field_name(a).starts_with(&format!("{fk}_"))
                    })
                    .collect();
                match hits.as_slice() {
                    [one] => one.1.clone(),
                    _ if vis.len() == 1 => vis[0].1.clone(),
                    _ => Value::Null,
                }
            }
            _ => Value::Null,
        }
    }
}

/// `row.title = v`: sets (or adds) a field of a record variable.
pub fn assign(vars: &mut HashMap<String, Value>, var: &str, v: Value) {
    let v = if var.contains('.') { v.scalar() } else { v };
    let Some((head, field)) = var.split_once('.') else {
        vars.insert(var.to_string(), v);
        return;
    };
    let rec = vars.entry(head.to_string()).or_insert(Value::Obj(vec![]));
    if !matches!(rec, Value::Obj(_)) {
        *rec = Value::Obj(vec![]);
    }
    if let Value::Obj(fs) = rec {
        match fs.iter_mut().find(|(k, _)| k == field) {
            Some(slot) => slot.1 = v,
            None => fs.push((field.to_string(), v)),
        }
    }
}

/// `row.title` → the field of a variable.
fn lookup(v: &str, vars: &HashMap<String, Value>) -> Result<Value, String> {
    let mut parts = v.split('.');
    let head = parts.next().unwrap_or(v);
    let mut cur = vars
        .get(head)
        .cloned()
        .ok_or_else(|| format!("{head} is not set"))?;
    for f in parts {
        cur = cur.get(f);
    }
    Ok(cur)
}

/// Evaluates an expression; `tests` holds the answers to its `test "…"`s, in order.
pub fn eval(
    e: &Expr,
    vars: &HashMap<String, Value>,
    effects: &mut std::collections::VecDeque<Value>,
) -> Result<Value, EvalError> {
    let tests = effects;
    Ok(match e {
        Expr::Num(n) => Value::Num(*n),
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Null => Value::Null,
        Expr::Str(s) => Value::Str(interpolate(s, vars)),
        Expr::Var(v) => lookup(v, vars)?,
        Expr::Effect(kind, _) => tests
            .pop_front()
            .ok_or(EvalError::MissingEffectResult { kind: *kind })?,
        Expr::Call(f, args) => {
            let a: Vec<Value> = args
                .iter()
                .map(|x| eval(x, vars, tests))
                .collect::<Result<_, _>>()?;
            let s0 = a.first().map(|v| v.to_string()).unwrap_or_default();
            match f {
                Builtin::Contains => {
                    let want = a
                        .get(1)
                        .map(|v| v.to_string())
                        .unwrap_or_default()
                        .to_lowercase();
                    Value::Bool(match a.first().map(Value::scalar) {
                        Some(Value::List(xs)) => {
                            xs.iter().any(|v| v.to_string().to_lowercase() == want)
                        }
                        _ => s0.to_lowercase().contains(&want),
                    })
                }
                Builtin::StartsWith => Value::Bool(
                    s0.to_lowercase().starts_with(
                        &a.get(1)
                            .map(|v| v.to_string())
                            .unwrap_or_default()
                            .to_lowercase(),
                    ),
                ),
                Builtin::Lower => Value::Str(s0.to_lowercase()),
                Builtin::Upper => Value::Str(s0.to_uppercase()),
                Builtin::Text => Value::Str(s0.trim().to_string()),
                Builtin::Number => a
                    .first()
                    .and_then(Value::num)
                    .map(Value::Num)
                    .unwrap_or(Value::Null),
                Builtin::Length => Value::Num(match a.first() {
                    Some(Value::List(xs)) => xs.len() as f64,
                    Some(Value::Null) | None => 0.0,
                    Some(v) => v.to_string().chars().count() as f64,
                }),
                Builtin::Empty => Value::Bool(!a.first().is_some_and(Value::truthy)),
            }
        }
        Expr::List(xs) => Value::List(
            xs.iter()
                .map(|x| eval(x, vars, tests))
                .collect::<Result<_, _>>()?,
        ),
        Expr::Not(x) => Value::Bool(!eval(x, vars, tests)?.truthy()),
        Expr::Bin(op, a, b) => {
            let a = eval(a, vars, tests)?.scalar();
            if *op == BinOp::And && !a.truthy() {
                return Ok(Value::Bool(false));
            }
            if *op == BinOp::Or && a.truthy() {
                return Ok(Value::Bool(true));
            }
            let b = eval(b, vars, tests)?.scalar();
            match op {
                BinOp::And | BinOp::Or => Value::Bool(b.truthy()),
                BinOp::Range => {
                    let (lo, hi) = (
                        a.num().unwrap_or(1.0) as i64,
                        b.num().ok_or("range needs a number")? as i64,
                    );
                    Value::List(
                        (lo..=hi.min(lo + 999))
                            .map(|n| Value::Num(n as f64))
                            .collect(),
                    )
                }
                // A list field compared with a value: membership.
                BinOp::Equal | BinOp::NotEqual
                    if matches!(a, Value::List(_)) != matches!(b, Value::List(_)) =>
                {
                    let (l, x) = if let Value::List(l) = &a {
                        (l, &b)
                    } else if let Value::List(l) = &b {
                        (l, &a)
                    } else {
                        unreachable!()
                    };
                    let hit = l.iter().any(|v| {
                        v.to_string()
                            .trim()
                            .eq_ignore_ascii_case(x.to_string().trim())
                    });
                    Value::Bool(hit == (*op == BinOp::Equal))
                }
                // A yes/no field compared with true/false: present means yes.
                BinOp::Equal | BinOp::NotEqual
                    if matches!(a, Value::Bool(_)) != matches!(b, Value::Bool(_)) =>
                {
                    Value::Bool((a.truthy() == b.truthy()) == (*op == BinOp::Equal))
                }
                BinOp::Equal | BinOp::NotEqual
                    if matches!(a, Value::Null) || matches!(b, Value::Null) =>
                {
                    let eq = !a.truthy()
                        && !b.truthy()
                        && (matches!(a, Value::Null | Value::Str(_))
                            && matches!(b, Value::Null | Value::Str(_)));
                    Value::Bool(eq == (*op == BinOp::Equal))
                }
                BinOp::Equal | BinOp::NotEqual => {
                    let eq = match (a.num(), b.num(), &a, &b) {
                        (Some(x), Some(y), Value::Num(_) | Value::Bool(_), _)
                        | (Some(x), Some(y), _, Value::Num(_) | Value::Bool(_)) => x == y,
                        _ => a
                            .to_string()
                            .trim()
                            .eq_ignore_ascii_case(b.to_string().trim()),
                    };
                    Value::Bool(eq == (*op == BinOp::Equal))
                }
                // A missing value (a field the page doesn't show) is neither
                // more nor less than anything.
                BinOp::Less | BinOp::LessEqual | BinOp::Greater | BinOp::GreaterEqual
                    if matches!(a, Value::Null) || matches!(b, Value::Null) =>
                {
                    Value::Bool(false)
                }
                BinOp::Less | BinOp::LessEqual | BinOp::Greater | BinOp::GreaterEqual => {
                    let (x, y) = (
                        a.num().ok_or_else(|| format!("\"{a}\" is not a number"))?,
                        b.num().ok_or_else(|| format!("\"{b}\" is not a number"))?,
                    );
                    Value::Bool(match op {
                        BinOp::Less => x < y,
                        BinOp::LessEqual => x <= y,
                        BinOp::Greater => x > y,
                        BinOp::GreaterEqual => x >= y,
                        _ => unreachable!("comparison branch"),
                    })
                }
                BinOp::Add if matches!(a, Value::List(_)) => {
                    let Value::List(mut xs) = a else {
                        unreachable!()
                    };
                    match b {
                        Value::List(ys) => xs.extend(ys),
                        other => xs.push(other),
                    }
                    Value::List(xs)
                }
                BinOp::Add => match (a.num(), b.num(), &a, &b) {
                    (Some(x), Some(y), Value::Num(_), Value::Num(_)) => Value::Num(x + y),
                    _ => Value::Str(format!("{a}{b}")),
                },
                BinOp::Subtract | BinOp::Multiply | BinOp::Divide | BinOp::Remainder => {
                    let (x, y) = (
                        a.num().ok_or_else(|| format!("\"{a}\" is not a number"))?,
                        b.num().ok_or_else(|| format!("\"{b}\" is not a number"))?,
                    );
                    Value::Num(match op {
                        BinOp::Subtract => x - y,
                        BinOp::Multiply => x * y,
                        BinOp::Divide if y != 0.0 => x / y,
                        BinOp::Remainder if y != 0.0 => x % y,
                        BinOp::Divide | BinOp::Remainder => return Err("division by zero".into()),
                        _ => unreachable!("arithmetic branch"),
                    })
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn deep_nesting_is_a_parse_error_not_a_crash() {
        let run = |src: String| std::thread::Builder::new().stack_size(2 << 20).spawn(move || parse(&src).map(|_| ()).map_err(|e| e.msg)).unwrap().join().unwrap();
        for src in [format!("x = {}1{}", "(".repeat(5000), ")".repeat(5000)), format!("x = {}1", "(".repeat(5000)), format!("emit {}a{}", "[".repeat(5000), "]".repeat(5000)), format!("x = {}1", "-".repeat(5000)), format!("x = {}true", "not ".repeat(5000))] {
            assert!(run(src).unwrap_err().contains("levels deep"));
        }
        assert!(run(format!("x = {}1{}", "(".repeat(20), ")".repeat(20))).is_ok());
        assert!(run(format!("do \"{}\"", "(".repeat(500))).is_ok(), "quoted text is not nesting");
    }

    #[test]
    fn string_escapes_include_line_breaks_and_tabs() {
        let step = |src: &str| match &parse(src).unwrap().ops[0] {
            Op::Leaf { text, .. } => text.clone(),
            op => panic!("{op:?}"),
        };
        assert_eq!(step(r#"do "one\n\ntwo\tthree \"q\" end""#), "one\n\ntwo\tthree \"q\" end");
        assert_eq!(step(r#"do 'a\nb'"#), "a\nb");
        // Other backslashes stay as written (a path, a pattern).
        assert_eq!(step(r#"do "open C:\\data\\x""#), r#"open C:\\data\\x"#);
        match &parse(r#"set t = "a\nb""#).unwrap().ops[0] {
            op => assert!(format!("{op:?}").contains(r#""a\nb""#), "{op:?}"),
        }
    }
    use super::*;

    #[test]
    fn validated_program_checkpoint_roundtrip() {
        let program = parse(HN).unwrap();
        let restored: Program =
            serde_json::from_str(&serde_json::to_string(&program).unwrap()).unwrap();
        assert_eq!(restored, program);
        assert_eq!(restored.instructions().len(), restored.ops().len());
        for (pc, instruction) in restored.instructions().enumerate() {
            assert_eq!(Some(instruction.line), restored.line(pc));
            assert_eq!(instruction.op, &restored.ops()[pc]);
        }
        assert_eq!(restored.line(restored.ops().len()), None);
    }

    #[test]
    fn checkpoint_rejects_invalid_structure() {
        let program = parse("set n = 1\nreturn n").unwrap();
        let valid = serde_json::to_value(&program).unwrap();
        for (field, value) in [
            ("lines", serde_json::json!([1])),
            ("lines", serde_json::json!([0, 2])),
            ("slots", serde_json::json!(99)),
        ] {
            let mut bad = valid.clone();
            bad[field] = value;
            assert!(serde_json::from_value::<Program>(bad).is_err());
        }
        let mut bad = valid.clone();
        bad["ops"][0] = serde_json::json!({"Jump": {"to": 3}});
        assert!(serde_json::from_value::<Program>(bad).is_err());
        let mut bad = valid;
        bad["ops"][0] = serde_json::json!({"Set": {"var": "n", "expr": {"Call": ["Unknown", []]}}});
        assert!(serde_json::from_value::<Program>(bad).is_err());
    }

    #[test]
    fn checkpoint_rejects_skipped_iterator_initialization() {
        let dto = ProgramDto {
            ops: vec![
                Op::Jump { to: 2 },
                Op::ForInit {
                    slot: 0,
                    list: Expr::List(vec![]),
                },
                Op::ForNext {
                    slot: 0,
                    var: "x".into(),
                    exit: 3,
                },
            ],
            lines: vec![1, 2, 3],
            slots: 1,
        };
        assert!(
            Program::try_from(dto)
                .unwrap_err()
                .msg
                .contains("before initialization")
        );
        let program =
            parse("for x in [1, 2]\n  if x == 1\n    set y = x\n  end\nend\nreturn 1").unwrap();
        assert_eq!(program.slots(), 1);
    }

    #[test]
    fn unknown_operations_rejected_even_after_return() {
        let error = parse("return 1\nset x = typo()").unwrap_err();
        assert_eq!(error.line, 2);
        assert!(error.msg.contains("unknown function typo()"));
        assert!(expr("1 ^ 2").is_err());
        assert!("invalid".parse::<BinOp>().is_err());
    }

    #[test]
    fn builtin_aliases_and_permissive_arguments_survive() {
        for (alias, canonical) in [
            ("includes", "contains"),
            ("has", "contains"),
            ("startswith", "starts_with"),
            ("str", "trim"),
            ("text", "trim"),
            ("num", "number"),
            ("int", "number"),
            ("float", "number"),
            ("count", "len"),
            ("is_empty", "empty"),
        ] {
            assert_eq!(
                alias.parse::<Builtin>().unwrap(),
                canonical.parse::<Builtin>().unwrap()
            );
        }
        let evaluate = |text| {
            eval(
                &expr(text).unwrap(),
                &HashMap::new(),
                &mut Default::default(),
            )
            .unwrap()
        };
        assert_eq!(evaluate("lower()"), Value::Str(String::new()));
        assert_eq!(evaluate("contains()"), Value::Bool(true));
        assert_eq!(evaluate("len(\"abc\", 123)"), Value::Num(3.0));
        assert_eq!(evaluate("number()"), Value::Null);
        let expression = expr("len(\"abc\", test \"extra argument\")").unwrap();
        assert_eq!(
            eval(&expression, &HashMap::new(), &mut Default::default()),
            Err(EvalError::MissingEffectResult { kind: Eff::Test })
        );
    }

    #[test]
    fn missing_effect_is_typed_and_short_circuit_does_not_consume() {
        let expression = expr("test \"ready\"").unwrap();
        assert_eq!(
            eval(&expression, &HashMap::new(), &mut Default::default()),
            Err(EvalError::MissingEffectResult { kind: Eff::Test })
        );
        let expression = expr("true or test \"unused\"").unwrap();
        let mut queue = std::collections::VecDeque::from([Value::Bool(false)]);
        assert_eq!(
            eval(&expression, &HashMap::new(), &mut queue).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(queue.len(), 1);
        // Collection order intentionally remains eager for the legacy runner fallback.
        let expression = expr("[true or test \"nested\", test \"last\"]").unwrap();
        assert_eq!(
            expression.effects(),
            vec![(Eff::Test, "nested".into()), (Eff::Test, "last".into())]
        );
    }

    #[test]
    fn checkpoint_values_preserve_float_bits_and_ordered_records() {
        for number in [
            0.0_f64,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from_bits(0x7ff8000000000042),
        ] {
            let original = Value::Obj(vec![
                ("number".into(), Value::Num(number)),
                ("item".into(), Value::Item(7)),
            ]);
            let restored: Value =
                serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
            let Value::Obj(fields) = restored else {
                panic!("lost record")
            };
            let Value::Num(restored) = fields[0].1 else {
                panic!("lost number")
            };
            assert_eq!(restored.to_bits(), number.to_bits());
            assert_eq!(fields[1], ("item".into(), Value::Item(7)));
        }
        assert!(serde_json::from_str::<Value>(r#"{"Num":null}"#).is_err());
    }

    const HN: &str = r#"set n = 1
while n <= 30
  do "open the comments of story $n on news.ycombinator.com"
  if test "the page shows a comment"
    read "the author of the first comment" -> author
    return author
  end
  set n = n + 1
end
fail "no story has comments""#;

    #[test]
    fn for_where_filters_items_by_their_own_fields() {
        // The list runs to the first `where` outside its quotes.
        let p = parse("for c in items \"comments where the score is high\" where c.pinned == null\n  emit c.author\nend").unwrap();
        assert_eq!(
            p.ops()[0],
            Op::ForInit {
                slot: 0,
                list: Expr::Effect(Eff::Items, "comments where the score is high".into())
            }
        );
        // The condition keeps its own meaning: a jump to the loop head is a
        // `continue`, so a rejected item is never bound to the body.
        let Op::JumpUnless { cond, to } = &p.ops()[2] else { panic!("expected the filter") };
        assert_eq!(p.ops()[*to], Op::ForNext { slot: 0, var: "c".into(), exit: 5 });
        assert_eq!(
            *cond,
            Expr::Not(Box::new(Expr::Bin(
                BinOp::Equal,
                Box::new(Expr::Var("c.pinned".into())),
                Box::new(Expr::Null)
            )))
        );
        // A list without a condition is unchanged.
        let plain = parse("for c in items \"comments\"\n  emit c.author\nend").unwrap();
        assert_eq!(plain.ops().len(), 4);
        assert!(!plain.ops().iter().any(|o| matches!(o, Op::JumpUnless { .. })));
    }

    #[test]
    fn for_where_error_cases() {
        // A `where` with no condition, a condition that doesn't parse, and one
        // that reads a name nothing has set, all name the line.
        let e = |src: &str| parse(src).unwrap_err();
        assert!(e("for c in items \"comments\" where\n  emit c.author\nend").msg.contains("where condition"));
        assert!(e("for c in items \"comments\" where c.pinned ==\n  emit c.author\nend").msg.contains("where condition"));
        let m = e("for c in items \"comments\" where d.pinned == null\n  emit c.author\nend");
        assert_eq!(m.line, 1);
        assert!(m.msg.contains("d is used before it is set"), "{}", m.msg);
        // `where` inside a word is part of the list, not a filter.
        let w = parse("for c in items \"comments\"\n  emit c.author\nend").unwrap();
        assert_eq!(w.ops()[0], Op::ForInit { slot: 0, list: Expr::Effect(Eff::Items, "comments".into()) });
        // `where` must be a whole word: a list that starts with `wherever` is
        // left whole, and fails as the list it is.
        assert!(e("for c in wherever \"comments\"\n  emit c.author\nend").msg.contains("unexpected"));
    }

    /// Runs a program with canned leaf answers (the page the loop finds on try 3).
    fn run(
        p: &Program,
        answer: impl Fn(Leaf, &str) -> Value,
    ) -> (Vec<String>, Result<Vec<Value>, String>) {
        let mut vars = HashMap::new();
        let mut iters: Vec<Vec<Value>> = vec![vec![]; p.slots];
        let mut pos = vec![0usize; p.slots];
        let mut log = vec![];
        let mut pc = 0;
        let mut budget = 1000;
        while pc < p.ops.len() {
            budget -= 1;
            assert!(budget > 0, "runaway");
            match &p.ops[pc] {
                Op::Leaf { kind, text, save } => {
                    let t = interpolate(text, &vars);
                    log.push(t.clone());
                    let v = answer(*kind, &t);
                    if let Some(s) = save {
                        vars.insert(s.clone(), v);
                    }
                }
                Op::Set { var, expr } => {
                    let v = eval(expr, &vars, &mut Default::default()).unwrap();
                    vars.insert(var.clone(), v);
                }
                Op::JumpUnless { cond, to } => {
                    let ts = cond
                        .effects()
                        .iter()
                        .map(|(_, q)| {
                            Value::Bool(answer(Leaf::Test, &interpolate(q, &vars)).truthy())
                        })
                        .collect();
                    let mut ts = ts;
                    if !eval(cond, &vars, &mut ts).unwrap().truthy() {
                        pc = *to;
                        continue;
                    }
                }
                Op::Jump { to } => {
                    pc = *to;
                    continue;
                }
                Op::ForInit { slot, list } => {
                    iters[*slot] = eval(list, &vars, &mut Default::default()).unwrap().items();
                    pos[*slot] = 0;
                }
                Op::ForNext { slot, var, exit } => {
                    if pos[*slot] >= iters[*slot].len() {
                        pc = *exit;
                        continue;
                    }
                    vars.insert(var.clone(), iters[*slot][pos[*slot]].clone());
                    pos[*slot] += 1;
                }
                Op::Fail(t) => return (log, Err(interpolate(t, &vars))),
                Op::Return(es) => {
                    return (
                        log,
                        Ok(es
                            .iter()
                            .map(|e| eval(e, &vars, &mut Default::default()).unwrap())
                            .collect()),
                    );
                }
                _ => {}
            }
            pc += 1;
        }
        (log, Ok(vec![]))
    }

    #[test]
    fn hn_loop() {
        let p = parse(HN).unwrap();
        let (log, out) = run(&p, |k, t| match k {
            Leaf::Test => Value::Bool(false),
            Leaf::Read => Value::Str("pg".into()),
            Leaf::Do => Value::Str(t.to_string()),
        });
        assert_eq!(out, Err("no story has comments".into()));
        assert_eq!(log.len(), 30);
        let (log, out) = run(&p, |k, _| match k {
            Leaf::Test => Value::Bool(true),
            Leaf::Read => Value::Str("pg".into()),
            Leaf::Do => Value::Str(String::new()),
        });
        assert_eq!(
            log,
            [
                "open the comments of story 1 on news.ycombinator.com",
                "the author of the first comment"
            ]
        );
        assert_eq!(out, Ok(vec![Value::Str("pg".into())]));
    }

    #[test]
    fn if_else_chain_and_numbers() {
        let src = r#"read "the amount due" -> due
if due > 200
  return due
elif due == 0
  fail "nothing due"
else
  do "pay $due from checking"
end"#;
        let p = parse(src).unwrap();
        let (log, out) = run(&p, |_, _| Value::Str("$1,250.00".into()));
        assert_eq!(out, Ok(vec![Value::Str("$1,250.00".into())]));
        assert_eq!(log.len(), 1);
        let (log, _) = run(&p, |_, _| Value::Str("$80".into()));
        assert_eq!(log[1], "pay $80 from checking");
    }

    #[test]
    fn for_each_and_break() {
        let src = r#"for o in 1042, 1043, 1050
  test "order $o has shipped" -> shipped
  if not shipped
    do "tell me order $o hasn't shipped"
    break
  end
end"#;
        let p = parse(src).unwrap();
        let (log, _) = run(&p, |k, t| {
            Value::Bool(!(k == Leaf::Test && t.contains("1043")))
        });
        assert_eq!(
            log,
            [
                "order 1042 has shipped",
                "order 1043 has shipped",
                "tell me order 1043 hasn't shipped"
            ]
        );
        // a read's text splits into items
        let p = parse(
            "read \"the unpaid invoices\" -> invs\nfor inv in $invs\n  do \"remind $inv\"\nend",
        )
        .unwrap();
        let (log, _) = run(&p, |k, _| {
            if k == Leaf::Read {
                Value::Str("INV-1\nINV-2".into())
            } else {
                Value::Bool(true)
            }
        });
        assert_eq!(&log[1..], ["remind INV-1", "remind INV-2"]);
    }

    #[test]
    fn lenient_forms() {
        let src = "```\n1. n = 1\nwhile n <= 3:\n  x = read \"price on day {n}\"\n  if x < 100 then\n    return x\n  endif\n  n = n + 1\nend\n```";
        let p = parse(src).unwrap();
        let (log, out) = run(&p, |_, t| {
            Value::Str(if t.ends_with('2') {
                "90 kr".into()
            } else {
                "120 kr".into()
            })
        });
        assert_eq!(log, ["price on day 1", "price on day 2"]);
        assert_eq!(out, Ok(vec![Value::Str("90 kr".into())]));
        let p = parse("repeat 3 times\n  do \"click Load more\"\nend").unwrap();
        assert_eq!(run(&p, |_, _| Value::Bool(true)).0.len(), 3);
    }

    /// Each permissive form and error reads as the hand-written parser read it
    /// (`script_cases.jsonl`, recorded from that parser before the grammar
    /// replaced it; `rejected` marks inputs it crashed on).
    #[test]
    fn permissive_forms_read_as_recorded() {
        for row in include_str!("script_cases.jsonl").lines() {
            let case: serde_json::Value = serde_json::from_str(row).unwrap();
            let src = case["src"].as_str().unwrap();
            let got = parse(src);
            if let Some(program) = case.get("program") {
                let want: Program = serde_json::from_value(program.clone()).unwrap();
                assert_eq!(got, Ok(want), "{src:?}");
            } else if let Some(error) = case.get("error") {
                let got = got.expect_err(src);
                assert_eq!(
                    (got.line, got.msg.as_str()),
                    (
                        error["line"].as_u64().unwrap() as usize,
                        error["msg"].as_str().unwrap()
                    ),
                    "{src:?}"
                );
            } else {
                assert!(got.is_err(), "{src:?}");
            }
        }
    }

    #[test]
    fn errors() {
        assert_eq!(parse("do \"open $x\"").unwrap_err().line, 1);
        assert!(
            parse("if test \"x\"\n  do \"y\"")
                .unwrap_err()
                .msg
                .contains("no end")
        );
        assert!(parse("break").is_err());
        assert!(
            parse("click Save")
                .unwrap_err()
                .msg
                .contains("unknown statement")
        );
        assert!(parse("\n# nothing\n").is_err());
        // secrets pass through
        let p = parse("do \"pay with {{Visa card number}}\"").unwrap();
        assert!(
            matches!(&p.ops[0], Op::Leaf { text, .. } if text.contains("{{Visa card number}}"))
        );
    }

    #[test]
    fn scraping_program() {
        let src = r#"while true
  for job in items "job listings"
    extract "title, company, Employment type" from job -> row
    open job "the job page"
    extract "salary" -> detail
    back
    if contains(row.company, "Acme") or detail.salary == null
      emit row, detail
    end
  end
  if not next page
    break
  end
end"#;
        let p = parse(src).unwrap();
        assert!(p.ops.iter().any(
            |o| matches!(o, Op::Open { leaf: true, how: Some(h), .. } if h == "the job page")
        ));
        assert!(p.ops.iter().any(|o| matches!(o, Op::Extract { fields, from: Some(f), .. } if f == "job" && fields[2] == "employment_type")));
        let nexts: Vec<_> = p
            .ops
            .iter()
            .filter_map(|o| {
                if let Op::JumpUnless { cond, .. } = o {
                    Some(cond.effects())
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        assert!(nexts.contains(&(Eff::Next, "page".into())));
        // a list read on the opened page can use the fetched page; paging there can't
        let p = parse("for c in items \"categories\"\n  open c\n  for b in items \"businesses\"\n    extract \"name\" from b -> r\n    emit r\n  end\n  back\nend").unwrap();
        assert!(
            p.ops
                .iter()
                .any(|o| matches!(o, Op::Open { leaf: true, .. }))
        );
        let p =
            parse("for c in items \"categories\"\n  open c\n  do \"sort by name\"\n  back\nend")
                .unwrap();
        assert!(
            p.ops
                .iter()
                .any(|o| matches!(o, Op::Open { leaf: false, .. }))
        );
        let v: HashMap<String, Value> = [(
            "row".into(),
            Value::Obj(vec![
                ("salary".into(), Value::Null),
                ("title".into(), Value::Str("SRE".into())),
            ]),
        )]
        .into();
        assert_eq!(
            expr("detail.\"prep time\"").unwrap(),
            Expr::Var("detail.prep_time".into())
        );
        assert!(matches!(
            expr("sec.section name == \"World\" and x").unwrap(),
            Expr::Bin(..)
        ));
        assert_eq!(
            expr("sec.section name").unwrap(),
            Expr::Var("sec.section_name".into())
        );
        assert_eq!(
            expr("detail[\"prep time\"]").unwrap(),
            Expr::Var("detail.prep_time".into())
        );
        let e = expr("row.salary == null and contains(row.title, \"sre\")").unwrap();
        assert_eq!(
            eval(&e, &v, &mut Default::default()).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn lints() {
        let p = parse("for x in items \"a\"\n  emit x\nend\nif next page\nend").unwrap();
        let l = lint(&p);
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(l[0].contains("outside a loop"));
        let p = parse("for x in items \"a\"\n  emit x\nend").unwrap();
        assert!(lint(&p)[0].contains("first page"));
        let p = parse("while true\n  for x in items \"a\"\n    emit x\n  end\n  if not next page\n    break\n  end\nend").unwrap();
        assert!(lint(&p).is_empty());
    }

    #[test]
    fn values() {
        assert_eq!(Value::Str("900 kr each".into()).num(), Some(900.0));
        assert_eq!(Value::Str("$1,250.00".into()).num(), Some(1250.0));
        assert!(!Value::Str("No".into()).truthy());
        assert!(Value::Str("Halvorsen Freight AS".into()).truthy());
        let v: HashMap<String, Value> = [
            ("a".into(), Value::Str("A".into())),
            ("ab".into(), Value::Num(2.0)),
        ]
        .into();
        assert_eq!(interpolate("$ab $a {a} {{a}}", &v), "2 A A {{a}}");
    }
}

/// Checkpoints preserve IEEE-754 bits; JSON null must never replace a nonfinite value.
mod float_bits {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.to_bits())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        Ok(f64::from_bits(u64::deserialize(deserializer)?))
    }
}
