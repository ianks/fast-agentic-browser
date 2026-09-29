//! A checkpointable, pure interpreter for [`crate::script::Program`].
//!
//! The machine owns program logic. A caller performs each page action and
//! returns its result with the capability carried by the request.

use crate::script::{self, BinOp, Eff, Expr, Leaf, Op, Program, Value};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, VecDeque};

const MAX_OPS: usize = 5_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionToken {
    epoch: [u8; 16],
    sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RequestKind {
    Leaf {
        kind: Leaf,
        text: String,
    },
    Effect {
        kind: Eff,
        query: String,
    },
    Extract {
        fields: Vec<String>,
        from: Option<Value>,
    },
    Open {
        item: Value,
        how: Option<String>,
        leaf: bool,
    },
    Back,
    ResolveItem {
        handle: u32,
    },
    /// The caller may supply another batch (for implicit pagination) or an
    /// empty batch to take the loop exit.
    ForNextExhausted {
        slot: usize,
        had_items: bool,
    },
}

impl RequestKind {
    /// Whether `response` has the shape this request promises: yes/no
    /// questions answer `Bool`, `items` answers a `List`, navigation answers
    /// `Done`, and an exhausted iterator answers a batch.
    pub fn accepts(&self, response: &Response) -> bool {
        match (self, response) {
            (RequestKind::Leaf { kind: Leaf::Test, .. }, Response::Value(Value::Bool(_))) => true,
            (RequestKind::Leaf { kind: Leaf::Test, .. }, _) => false,
            (
                RequestKind::Effect {
                    kind: Eff::Test | Eff::Next,
                    ..
                },
                Response::Value(Value::Bool(_)),
            ) => true,
            (
                RequestKind::Effect {
                    kind: Eff::Items, ..
                },
                Response::Value(Value::List(_)),
            ) => true,
            (RequestKind::Effect { .. }, _) => false,
            (
                RequestKind::Leaf { .. } | RequestKind::Extract { .. } | RequestKind::ResolveItem { .. },
                Response::Value(_),
            ) => true,
            (RequestKind::Open { .. } | RequestKind::Back, Response::Done) => true,
            (RequestKind::ForNextExhausted { .. }, Response::IteratorItems(_)) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub token: CompletionToken,
    pub line: usize,
    pub kind: RequestKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Value(Value),
    Done,
    IteratorItems(Vec<Value>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    Returned(Vec<Value>),
    Failed { line: usize, reason: String },
    Ended,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Request(Request),
    Record(serde_json::Value),
    Finished(Outcome),
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum MachineError {
    #[error("could not create completion capability: {0}")]
    Random(#[from] getrandom::Error),
    #[error("completion does not match the suspended request")]
    StaleCompletion,
    #[error("completion has the wrong response kind")]
    WrongResponseKind,
    #[error("the machine has no pending request")]
    NotSuspended,
    #[error("invalid checkpoint: {0}")]
    InvalidCheckpoint(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct IteratorState {
    values: Vec<Value>,
    next: usize,
    initialized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum EvalTarget {
    Set(String),
    JumpUnless(usize),
    ForInit(usize),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EvalProgress {
    target: EvalTarget,
    answers: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum PendingAction {
    Leaf(Option<String>),
    Effect,
    Extract(String),
    Open,
    Back,
    ResolveItem(String),
    ForNextExhausted { slot: usize, exit: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Pending {
    request: Request,
    action: PendingAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineDto {
    program: Program,
    pc: usize,
    vars: HashMap<String, Value>,
    iterators: Vec<IteratorState>,
    pending: Option<Pending>,
    evaluation: Option<EvalProgress>,
    outcome: Option<Outcome>,
    executed: usize,
    epoch: [u8; 16],
    next_sequence: u64,
}

/// A resumable interpreter. Its fields and deserialization are validated so
/// checkpoints cannot contain an out-of-bounds program counter or iterator.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramMachine(MachineDto);

impl ProgramMachine {
    pub fn new(program: Program) -> Result<Self, MachineError> {
        let mut epoch = [0; 16];
        getrandom::fill(&mut epoch)?;
        Ok(Self(MachineDto {
            iterators: vec![
                IteratorState {
                    values: vec![],
                    next: 0,
                    initialized: false
                };
                program.slots()
            ],
            program,
            pc: 0,
            vars: HashMap::new(),
            pending: None,
            evaluation: None,
            outcome: None,
            executed: 0,
            epoch,
            next_sequence: 0,
        }))
    }

    pub fn program(&self) -> &Program {
        &self.0.program
    }
    pub fn pc(&self) -> usize {
        self.0.pc
    }
    pub fn vars(&self) -> &HashMap<String, Value> {
        &self.0.vars
    }
    pub fn pending(&self) -> Option<&Request> {
        self.0.pending.as_ref().map(|p| &p.request)
    }

    /// Every page-item handle the machine holds, so a caller that restores
    /// the item store alongside can check the two agree.
    pub fn item_handles(&self) -> Vec<u32> {
        fn walk(value: &Value, out: &mut Vec<u32>) {
            match value {
                Value::Item(handle) => out.push(*handle),
                Value::List(values) => values.iter().for_each(|v| walk(v, out)),
                Value::Obj(fields) => fields.iter().for_each(|(_, v)| walk(v, out)),
                _ => {}
            }
        }
        let mut out = vec![];
        self.0.vars.values().for_each(|v| walk(v, &mut out));
        self.0.iterators.iter().flat_map(|it| &it.values).for_each(|v| walk(v, &mut out));
        if let Some(progress) = &self.0.evaluation {
            progress.answers.iter().for_each(|v| walk(v, &mut out));
        }
        if let Some(pending) = &self.0.pending {
            match &pending.request.kind {
                RequestKind::ResolveItem { handle } => out.push(*handle),
                RequestKind::Open { item, .. } => walk(item, &mut out),
                RequestKind::Extract { from: Some(from), .. } => walk(from, &mut out),
                _ => {}
            }
        }
        out
    }

    /// Run pure instructions until a page request, record, or terminal result.
    /// Calling this again while suspended returns the same request.
    pub fn advance(&mut self) -> Event {
        loop {
            if let Some(outcome) = &self.0.outcome {
                return Event::Finished(outcome.clone());
            }
            if let Some(pending) = &self.0.pending {
                return Event::Request(pending.request.clone());
            }
            if self.0.pc == self.0.program.ops().len() {
                return self.finish(Outcome::Ended);
            }
            if self.0.executed >= MAX_OPS {
                return self.fail("the instruction budget was exceeded".into());
            }
            if self.0.evaluation.is_some() {
                if let Some(event) = self.advance_evaluation() {
                    return event;
                }
                continue;
            }
            self.0.executed += 1;
            let op = self.0.program.ops()[self.0.pc].clone();
            match op {
                Op::Leaf { kind, text, save } => {
                    return self.request(
                        RequestKind::Leaf {
                            kind,
                            text: script::interpolate(&text, &self.0.vars),
                        },
                        PendingAction::Leaf(save),
                    );
                }
                Op::Set { var, .. } => {
                    self.0.evaluation = Some(EvalProgress {
                        target: EvalTarget::Set(var),
                        answers: vec![],
                    })
                }
                Op::JumpUnless { to, .. } => {
                    self.0.evaluation = Some(EvalProgress {
                        target: EvalTarget::JumpUnless(to),
                        answers: vec![],
                    })
                }
                Op::ForInit { slot, .. } => {
                    self.0.evaluation = Some(EvalProgress {
                        target: EvalTarget::ForInit(slot),
                        answers: vec![],
                    })
                }
                Op::Jump { to } => self.0.pc = to,
                Op::ForNext { slot, var, exit } => {
                    let iterator = &mut self.0.iterators[slot];
                    if !iterator.initialized {
                        return self.fail("iterator was not initialized".into());
                    }
                    if iterator.next >= iterator.values.len() {
                        let had_items = !iterator.values.is_empty();
                        return self.request(
                            RequestKind::ForNextExhausted { slot, had_items },
                            PendingAction::ForNextExhausted { slot, exit },
                        );
                    } else {
                        let item = iterator.values[iterator.next].clone();
                        iterator.next += 1;
                        match item {
                            Value::Item(handle) => {
                                return self.request(
                                    RequestKind::ResolveItem { handle },
                                    PendingAction::ResolveItem(var),
                                );
                            }
                            value => {
                                self.0.vars.insert(var, value);
                                self.0.pc += 1;
                            }
                        }
                    }
                }
                Op::Fail(text) => {
                    return self.finish(Outcome::Failed {
                        line: self.line(),
                        reason: script::interpolate(&text, &self.0.vars),
                    });
                }
                Op::Return(expressions) => {
                    let values = expressions
                        .iter()
                        .map(|expr| script::eval(expr, &self.0.vars, &mut VecDeque::new()))
                        .collect::<Result<Vec<_>, _>>();
                    return match values {
                        Ok(values) => self.finish(Outcome::Returned(values)),
                        Err(error) => self.fail(error.to_string()),
                    };
                }
                Op::Extract { fields, from, save } => {
                    let source = match from {
                        Some(name) => match self.0.vars.get(&name) {
                            Some(value) => Some(value.clone()),
                            None => return self.fail(format!("{name} is not set")),
                        },
                        None => None,
                    };
                    return self.request(
                        RequestKind::Extract {
                            fields,
                            from: source,
                        },
                        PendingAction::Extract(save),
                    );
                }
                Op::Open { item, how, leaf } => {
                    let value = match self.0.vars.get(&item) {
                        Some(value) => value.clone(),
                        None => return self.fail(format!("{item} is not set")),
                    };
                    return self.request(
                        RequestKind::Open {
                            item: value,
                            how,
                            leaf,
                        },
                        PendingAction::Open,
                    );
                }
                Op::Back => return self.request(RequestKind::Back, PendingAction::Back),
                Op::Emit(expressions) => match emit_record(&expressions, &self.0.vars) {
                    Ok(record) => {
                        self.0.pc += 1;
                        return Event::Record(record);
                    }
                    Err(error) => return self.fail(error),
                },
            }
        }
    }

    /// Complete exactly the outstanding request. A failed page action becomes
    /// a terminal, line-located failure; retrying the same token is rejected.
    pub fn complete(
        &mut self,
        token: &CompletionToken,
        response: Result<Response, String>,
    ) -> Result<(), MachineError> {
        let pending = self.0.pending.as_ref().ok_or(MachineError::NotSuspended)?;
        if &pending.request.token != token {
            return Err(MachineError::StaleCompletion);
        }
        if let Ok(ref response) = response {
            if !pending.request.kind.accepts(response) {
                return Err(MachineError::WrongResponseKind);
            }
        }
        let pending = self.0.pending.take().expect("checked above");
        let (value, items) = match response {
            Err(reason) => {
                self.finish(Outcome::Failed {
                    line: self.line(),
                    reason,
                });
                return Ok(());
            }
            Ok(Response::Value(value)) => (Some(value), None),
            Ok(Response::Done) => (None, None),
            Ok(Response::IteratorItems(items)) => (None, Some(items)),
        };
        match pending.action {
            PendingAction::Leaf(save) => {
                if let Some(name) = save {
                    self.0.vars.insert(name, value.expect("value response"));
                }
                self.0.pc += 1;
            }
            PendingAction::Effect => self
                .0
                .evaluation
                .as_mut()
                .expect("validated evaluation")
                .answers
                .push(value.expect("value response")),
            PendingAction::Extract(name) | PendingAction::ResolveItem(name) => {
                self.0.vars.insert(name, value.expect("value response"));
                self.0.pc += 1;
            }
            PendingAction::Open | PendingAction::Back => self.0.pc += 1,
            PendingAction::ForNextExhausted { slot, exit } => {
                let items = items.expect("iterator response");
                if items.is_empty() {
                    self.0.pc = exit;
                } else {
                    self.0.iterators[slot] = IteratorState {
                        values: items,
                        next: 0,
                        initialized: true,
                    };
                }
            }
        }
        Ok(())
    }

    fn advance_evaluation(&mut self) -> Option<Event> {
        let progress = self.0.evaluation.as_ref().expect("evaluation present");
        let expression = match &self.0.program.ops()[self.0.pc] {
            Op::Set { expr, .. } => expr,
            Op::JumpUnless { cond, .. } => cond,
            Op::ForInit { list, .. } => list,
            _ => unreachable!("validated evaluation target"),
        };
        let mut used = 0;
        match eval_with_replay(expression, &self.0.vars, &progress.answers, &mut used) {
            Ok(Walk::Need(kind, query)) => {
                Some(self.request(RequestKind::Effect { kind, query }, PendingAction::Effect))
            }
            Ok(Walk::Ready(value)) => {
                let progress = self.0.evaluation.take().expect("evaluation present");
                if used != progress.answers.len() {
                    return Some(self.fail("effect answers do not match the expression".into()));
                }
                match progress.target {
                    EvalTarget::Set(var) => {
                        script::assign(&mut self.0.vars, &var, value);
                        self.0.pc += 1;
                    }
                    EvalTarget::JumpUnless(to) => {
                        self.0.pc = if value.truthy() { self.0.pc + 1 } else { to }
                    }
                    EvalTarget::ForInit(slot) => {
                        self.0.iterators[slot] = IteratorState {
                            values: value.items_or_handles(),
                            next: 0,
                            initialized: true,
                        };
                        self.0.pc += 1;
                    }
                }
                None
            }
            Err(error) => Some(self.fail(error)),
        }
    }

    fn request(&mut self, kind: RequestKind, action: PendingAction) -> Event {
        if self.0.next_sequence == u64::MAX {
            return self.fail("completion sequence was exhausted".into());
        }
        let token = CompletionToken {
            epoch: self.0.epoch,
            sequence: self.0.next_sequence,
        };
        self.0.next_sequence += 1;
        let request = Request {
            token,
            line: self.line(),
            kind,
        };
        self.0.pending = Some(Pending {
            request: request.clone(),
            action,
        });
        Event::Request(request)
    }
    fn line(&self) -> usize {
        self.0.program.line(self.0.pc).unwrap_or(1)
    }
    fn fail(&mut self, reason: String) -> Event {
        self.finish(Outcome::Failed {
            line: self.line(),
            reason,
        })
    }
    fn finish(&mut self, outcome: Outcome) -> Event {
        self.0.pending = None;
        self.0.evaluation = None;
        self.0.outcome = Some(outcome.clone());
        Event::Finished(outcome)
    }
}

enum Walk {
    Ready(Value),
    Need(Eff, String),
}

/// Replay completed answers against the current immutable expression. The
/// runner eagerly performs effects in non-boolean expressions, including
/// effects inside nested boolean expressions; top-level boolean operators
/// and `not` evaluate their children conditionally.
fn eval_with_replay(
    e: &Expr,
    vars: &HashMap<String, Value>,
    answers: &[Value],
    used: &mut usize,
) -> Result<Walk, String> {
    if e.effects().is_empty() {
        return script::eval(e, vars, &mut VecDeque::new())
            .map(Walk::Ready)
            .map_err(|e| e.to_string());
    }
    match e {
        Expr::Effect(kind, query) => {
            if let Some(value) = answers.get(*used) {
                *used += 1;
                let shaped = match kind {
                    Eff::Test | Eff::Next => matches!(value, Value::Bool(_)),
                    Eff::Items => matches!(value, Value::List(_)),
                };
                if !shaped {
                    return Err(format!("{kind:?} answer has the wrong shape"));
                }
                Ok(Walk::Ready(value.clone()))
            } else {
                Ok(Walk::Need(*kind, script::interpolate(query, vars)))
            }
        }
        Expr::Not(inner) => match eval_with_replay(inner, vars, answers, used)? {
            Walk::Ready(value) => Ok(Walk::Ready(Value::Bool(!value.scalar().truthy()))),
            need => Ok(need),
        },
        Expr::Bin(op @ (BinOp::And | BinOp::Or), left, right) => {
            let left = match eval_with_replay(left, vars, answers, used)? {
                Walk::Ready(value) => value.scalar().truthy(),
                need => return Ok(need),
            };
            if (*op == BinOp::And && !left) || (*op == BinOp::Or && left) {
                return Ok(Walk::Ready(Value::Bool(left)));
            }
            match eval_with_replay(right, vars, answers, used)? {
                Walk::Ready(value) => Ok(Walk::Ready(Value::Bool(value.scalar().truthy()))),
                need => Ok(need),
            }
        }
        _ => {
            let mut queue = VecDeque::new();
            for (kind, query) in e.effects() {
                if let Some(value) = answers.get(*used) {
                    queue.push_back(value.clone());
                    *used += 1;
                } else {
                    return Ok(Walk::Need(kind, script::interpolate(&query, vars)));
                }
            }
            script::eval(e, vars, &mut queue)
                .map(Walk::Ready)
                .map_err(|e| e.to_string())
        }
    }
}

fn emit_record(
    expressions: &[(Option<String>, Expr)],
    vars: &HashMap<String, Value>,
) -> Result<serde_json::Value, String> {
    let mut rec = serde_json::Map::new();
    let bare: Vec<&Expr> = expressions.iter().map(|(_, expr)| expr).collect();
    for (key, expr) in expressions {
        let value =
            script::eval(expr, vars, &mut VecDeque::new()).map_err(|error| error.to_string())?;
        if let Some(key) = key {
            rec.insert(key.clone(), value.scalar().to_json());
            continue;
        }
        let value = if matches!(expr, Expr::Var(name) if name.contains('.')) {
            value.scalar()
        } else {
            value
        };
        match value {
            Value::Obj(fields) => {
                for (key, value) in fields {
                    if !key.starts_with("__") {
                        rec.insert(key, value.to_json());
                    }
                }
            }
            other => {
                let key = match expr {
                    Expr::Var(name) => {
                        let (head, field) = name.rsplit_once('.').unwrap_or(("", name));
                        let head = head.rsplit('.').next().unwrap_or(head);
                        if !rec.contains_key(field) && !bare.iter().any(|x| matches!(x, Expr::Var(m) if m != name && m.rsplit('.').next() == Some(field))) { field.to_string() }
                        else if !head.is_empty() && !rec.contains_key(head) && !["row", "r", "item", "detail", "d", "data", "rec", "record", "info"].contains(&head) { head.to_string() }
                        else { format!("{head}_{field}") }
                    }
                    _ => format!("value{}", rec.len() + 1),
                };
                rec.insert(key, other.to_json());
            }
        }
    }
    Ok(serde_json::Value::Object(rec))
}

impl TryFrom<MachineDto> for ProgramMachine {
    type Error = MachineError;
    fn try_from(dto: MachineDto) -> Result<Self, Self::Error> {
        let invalid = |message: &str| MachineError::InvalidCheckpoint(message.into());
        if dto.pc > dto.program.ops().len() {
            return Err(invalid("program counter is out of bounds"));
        }
        if dto.iterators.len() != dto.program.slots() {
            return Err(invalid("iterator count does not match program"));
        }
        if dto.iterators.iter().any(|it| {
            it.next > it.values.len()
                || (!it.initialized && (!it.values.is_empty() || it.next != 0))
        }) {
            return Err(invalid("invalid iterator state"));
        }
        if dto.pc < dto.program.ops().len()
            && matches!(&dto.program.ops()[dto.pc], Op::ForNext { slot, .. } if !dto.iterators[*slot].initialized)
            && dto.outcome.is_none()
        {
            return Err(invalid("current iterator is not initialized"));
        }
        if dto.outcome.is_some() && (dto.pending.is_some() || dto.evaluation.is_some()) {
            return Err(invalid("finished machine has pending work"));
        }
        if dto.pc == dto.program.ops().len() && (dto.pending.is_some() || dto.evaluation.is_some())
        {
            return Err(invalid("pending work is past the program end"));
        }
        if let Some(eval) = &dto.evaluation {
            let matches = matches!((&eval.target, &dto.program.ops()[dto.pc]),
                (EvalTarget::Set(a), Op::Set { var: b, .. }) if a == b)
                || matches!((&eval.target, &dto.program.ops()[dto.pc]), (EvalTarget::JumpUnless(a), Op::JumpUnless { to: b, .. }) if a == b)
                || matches!((&eval.target, &dto.program.ops()[dto.pc]), (EvalTarget::ForInit(a), Op::ForInit { slot: b, .. }) if a == b);
            if !matches {
                return Err(invalid("evaluation does not match current instruction"));
            }
            let expression = match &dto.program.ops()[dto.pc] {
                Op::Set { expr, .. } => expr,
                Op::JumpUnless { cond, .. } => cond,
                Op::ForInit { list, .. } => list,
                _ => unreachable!(),
            };
            if eval.answers.len() > expression.effects().len() {
                return Err(invalid("too many expression answers"));
            }
        }
        if let Some(pending) = &dto.pending {
            if pending.request.token.epoch != dto.epoch
                || pending.request.token.sequence >= dto.next_sequence
                || pending.request.line != dto.program.line(dto.pc).unwrap_or(1)
            {
                return Err(invalid("invalid pending capability"));
            }
            let valid = match (
                &pending.action,
                &pending.request.kind,
                &dto.program.ops()[dto.pc],
            ) {
                (
                    PendingAction::Leaf(save),
                    RequestKind::Leaf { kind, text },
                    Op::Leaf {
                        kind: expected_kind,
                        text: source,
                        save: expected_save,
                    },
                ) => {
                    save == expected_save
                        && kind == expected_kind
                        && text == &script::interpolate(source, &dto.vars)
                }
                (PendingAction::Effect, RequestKind::Effect { kind, query }, _) => {
                    if let Some(progress) = &dto.evaluation {
                        let expression = match &dto.program.ops()[dto.pc] {
                            Op::Set { expr, .. } => expr,
                            Op::JumpUnless { cond, .. } => cond,
                            Op::ForInit { list, .. } => list,
                            _ => unreachable!(),
                        };
                        let mut used = 0;
                        matches!(eval_with_replay(expression, &dto.vars, &progress.answers, &mut used), Ok(Walk::Need(k, q)) if k == *kind && q == *query && used == progress.answers.len())
                    } else {
                        false
                    }
                }
                (
                    PendingAction::Extract(save),
                    RequestKind::Extract { fields, from },
                    Op::Extract {
                        fields: expected_fields,
                        from: source,
                        save: expected_save,
                    },
                ) => {
                    save == expected_save
                        && fields == expected_fields
                        && from == &source.as_ref().and_then(|name| dto.vars.get(name).cloned())
                }
                (
                    PendingAction::Open,
                    RequestKind::Open { item, how, leaf },
                    Op::Open {
                        item: name,
                        how: expected_how,
                        leaf: expected_leaf,
                    },
                ) => {
                    dto.vars.get(name) == Some(item) && how == expected_how && leaf == expected_leaf
                }
                (PendingAction::Back, RequestKind::Back, Op::Back) => true,
                (
                    PendingAction::ResolveItem(var),
                    RequestKind::ResolveItem { handle },
                    Op::ForNext {
                        slot,
                        var: expected_var,
                        ..
                    },
                ) => {
                    let it = &dto.iterators[*slot];
                    var == expected_var
                        && it.initialized
                        && it.next > 0
                        && it.values[it.next - 1] == Value::Item(*handle)
                }
                (
                    PendingAction::ForNextExhausted { slot, exit },
                    RequestKind::ForNextExhausted {
                        slot: requested,
                        had_items,
                    },
                    Op::ForNext {
                        slot: expected,
                        exit: expected_exit,
                        ..
                    },
                ) => {
                    let it = &dto.iterators[*slot];
                    slot == requested
                        && slot == expected
                        && exit == expected_exit
                        && it.initialized
                        && it.next >= it.values.len()
                        && *had_items == !it.values.is_empty()
                }
                _ => false,
            };
            if !valid {
                return Err(invalid(
                    "pending request does not match current instruction",
                ));
            }
        } else if dto
            .evaluation
            .as_ref()
            .is_some_and(|e| !e.answers.is_empty())
        {
            let progress = dto.evaluation.as_ref().expect("checked");
            let expression = match &dto.program.ops()[dto.pc] {
                Op::Set { expr, .. } => expr,
                Op::JumpUnless { cond, .. } => cond,
                Op::ForInit { list, .. } => list,
                _ => unreachable!(),
            };
            let mut used = 0;
            if eval_with_replay(expression, &dto.vars, &progress.answers, &mut used).is_err()
                || used != progress.answers.len()
            {
                return Err(invalid(
                    "expression answers do not match current instruction",
                ));
            }
        }
        Ok(Self(dto))
    }
}

impl Serialize for ProgramMachine {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for ProgramMachine {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(MachineDto::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(machine: &mut ProgramMachine) -> Request {
        match machine.advance() {
            Event::Request(request) => request,
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn suspends_resumes_and_rejects_stale_completion() {
        let program = script::parse(
            "read \"number\" -> x\nif x > 2 and test \"ready\"\n  emit value: x\nend\nreturn x",
        )
        .unwrap();
        let mut machine = ProgramMachine::new(program).unwrap();
        let first = request(&mut machine);
        assert_eq!(
            first.kind,
            RequestKind::Leaf {
                kind: Leaf::Read,
                text: "number".into()
            }
        );
        assert_eq!(request(&mut machine), first);
        machine
            .complete(&first.token, Ok(Response::Value(Value::Str("3".into()))))
            .unwrap();
        let second = request(&mut machine);
        assert_eq!(
            second.kind,
            RequestKind::Effect {
                kind: Eff::Test,
                query: "ready".into()
            }
        );
        assert_eq!(
            machine.complete(&first.token, Ok(Response::Value(Value::Bool(true)))),
            Err(MachineError::StaleCompletion)
        );
        let checkpoint = serde_json::to_string(&machine).unwrap();
        let mut machine: ProgramMachine = serde_json::from_str(&checkpoint).unwrap();
        assert_eq!(request(&mut machine), second);
        machine
            .complete(&second.token, Ok(Response::Value(Value::Bool(true))))
            .unwrap();
        assert_eq!(
            machine.advance(),
            Event::Record(serde_json::json!({"value": "3"}))
        );
        assert_eq!(
            machine.advance(),
            Event::Finished(Outcome::Returned(vec![Value::Str("3".into())]))
        );
    }

    #[test]
    fn short_circuit_and_eager_nested_effects_match_runner() {
        let mut machine = ProgramMachine::new(script::parse("set x = true or test \"unused\"\nset y = [true or test \"eager\", test \"last\"]\nreturn y").unwrap()).unwrap();
        let first = request(&mut machine);
        assert_eq!(
            first.kind,
            RequestKind::Effect {
                kind: Eff::Test,
                query: "eager".into()
            }
        );
        machine
            .complete(&first.token, Ok(Response::Value(Value::Bool(false))))
            .unwrap();
        let second = request(&mut machine);
        assert_eq!(
            second.kind,
            RequestKind::Effect {
                kind: Eff::Test,
                query: "last".into()
            }
        );
        machine
            .complete(&second.token, Ok(Response::Value(Value::Bool(true))))
            .unwrap();
        assert_eq!(
            machine.advance(),
            Event::Finished(Outcome::Returned(vec![Value::List(vec![
                Value::Bool(true),
                Value::Bool(false)
            ])]))
        );
    }

    #[test]
    fn rejects_tampered_checkpoint_and_wrong_completion() {
        let mut machine = ProgramMachine::new(script::parse("do \"go\"").unwrap()).unwrap();
        let pending = request(&mut machine);
        assert_eq!(
            machine.complete(&pending.token, Ok(Response::Done)),
            Err(MachineError::WrongResponseKind)
        );
        let mut json = serde_json::to_value(&machine).unwrap();
        json["pc"] = serde_json::json!(99);
        assert!(serde_json::from_value::<ProgramMachine>(json).is_err());
        machine
            .complete(&pending.token, Ok(Response::Value(Value::Null)))
            .unwrap();
        assert_eq!(machine.advance(), Event::Finished(Outcome::Ended));
        assert_eq!(
            machine.complete(&pending.token, Ok(Response::Value(Value::Null))),
            Err(MachineError::NotSuspended)
        );
    }

    #[test]
    fn completions_must_have_the_promised_shape() {
        let program =
            script::parse("test \"ready\" -> ok\nif test \"a\"\n  emit a: 1\nend\nfor x in items \"rows\"\n  emit x\nend").unwrap();
        let mut machine = ProgramMachine::new(program).unwrap();
        let leaf = request(&mut machine);
        assert_eq!(
            machine.complete(&leaf.token, Ok(Response::Value(Value::Str("yes".into())))),
            Err(MachineError::WrongResponseKind)
        );
        assert_eq!(machine.pending(), Some(&leaf), "rejected completion keeps the request");
        machine.complete(&leaf.token, Ok(Response::Value(Value::Bool(true)))).unwrap();
        let test = request(&mut machine);
        assert!(matches!(test.kind, RequestKind::Effect { kind: Eff::Test, .. }));
        assert_eq!(
            machine.complete(&test.token, Ok(Response::Value(Value::Num(1.0)))),
            Err(MachineError::WrongResponseKind)
        );
        machine.complete(&test.token, Ok(Response::Value(Value::Bool(false)))).unwrap();
        let items = request(&mut machine);
        assert!(matches!(items.kind, RequestKind::Effect { kind: Eff::Items, .. }));
        assert_eq!(
            machine.complete(&items.token, Ok(Response::Value(Value::Bool(true)))),
            Err(MachineError::WrongResponseKind)
        );
        assert_eq!(
            machine.complete(&items.token, Ok(Response::Done)),
            Err(MachineError::WrongResponseKind)
        );
        machine.complete(&items.token, Ok(Response::Value(Value::List(vec![])))).unwrap();
    }

    #[test]
    fn checkpoint_rejects_mis_shaped_expression_answers() {
        let program = script::parse("if test \"a\" and test \"b\"\n  emit a: 1\nend").unwrap();
        let mut machine = ProgramMachine::new(program).unwrap();
        let first = request(&mut machine);
        machine.complete(&first.token, Ok(Response::Value(Value::Bool(true)))).unwrap();
        request(&mut machine);
        let mut json = serde_json::to_value(&machine).unwrap();
        assert!(serde_json::from_value::<ProgramMachine>(json.clone()).is_ok());
        json["evaluation"]["answers"][0] = serde_json::to_value(Value::Str("yes".into())).unwrap();
        assert!(serde_json::from_value::<ProgramMachine>(json).is_err());
    }

    #[test]
    fn iterator_extension_and_item_materialization_are_explicit() {
        let program = script::parse("for item in [1]\n  emit item\nend\nreturn 0").unwrap();
        let mut machine = ProgramMachine::new(program).unwrap();
        assert_eq!(
            machine.advance(),
            Event::Record(serde_json::json!({"item": 1}))
        );
        let exhausted = request(&mut machine);
        assert_eq!(
            exhausted.kind,
            RequestKind::ForNextExhausted {
                slot: 0,
                had_items: true
            }
        );
        machine
            .complete(
                &exhausted.token,
                Ok(Response::IteratorItems(vec![Value::Item(7)])),
            )
            .unwrap();
        let hydrate = request(&mut machine);
        assert_eq!(hydrate.kind, RequestKind::ResolveItem { handle: 7 });
        machine
            .complete(
                &hydrate.token,
                Ok(Response::Value(Value::Obj(vec![(
                    "title".into(),
                    Value::Str("next".into()),
                )]))),
            )
            .unwrap();
        assert_eq!(
            machine.advance(),
            Event::Record(serde_json::json!({"title": "next"}))
        );
        let exhausted = request(&mut machine);
        machine
            .complete(&exhausted.token, Ok(Response::IteratorItems(vec![])))
            .unwrap();
        assert_eq!(
            machine.advance(),
            Event::Finished(Outcome::Returned(vec![Value::Num(0.0)]))
        );
    }

    #[test]
    fn failed_effect_checkpoint_is_terminal() {
        let mut machine =
            ProgramMachine::new(script::parse("set x = test \"ready\"\nreturn x").unwrap())
                .unwrap();
        let effect = request(&mut machine);
        machine
            .complete(&effect.token, Err("page unavailable".into()))
            .unwrap();
        let restored: ProgramMachine =
            serde_json::from_str(&serde_json::to_string(&machine).unwrap()).unwrap();
        assert_eq!(restored.0.evaluation, None);
        assert_eq!(restored.0.pending, None);
        assert_eq!(
            machine.advance(),
            Event::Finished(Outcome::Failed {
                line: 1,
                reason: "page unavailable".into()
            })
        );
    }
}
