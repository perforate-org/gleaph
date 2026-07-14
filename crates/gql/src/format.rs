//! AST-based formatting for the supported GQL read-query surface.
//!
//! Formatting is deliberately derived from the AST. Unsupported nodes return an
//! error instead of being silently omitted or reconstructed with text heuristics.

use std::fmt;

use crate::Value;
use crate::ast::*;
use crate::parser;
use crate::types::{EdgeDirection, LabelExpr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeywordCase {
    Upper,
    Lower,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClauseBreakPolicy {
    EveryClause,
    Compact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemBreakPolicy {
    EveryItem,
    Compact,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatOptions {
    /// Repeated for each nesting level. Must be non-empty.
    pub indentation: String,
    /// Maximum preferred width for emitted lines. The formatter wraps at
    /// clause and projection-item boundaries when possible, but does not split
    /// an individual expression or graph pattern to satisfy this limit.
    pub line_width: usize,
    /// Casing used for formatter-emitted keywords.
    pub keyword_case: KeywordCase,
    /// Whether clauses are separated by newlines or spaces.
    pub clause_breaks: ClauseBreakPolicy,
    /// In multiline projection lists, put commas after items when true, or at
    /// the start of the following item line when false.
    pub comma_after_break: bool,
    /// Whether result projections are one item per line or compact.
    pub result_item_breaks: ItemBreakPolicy,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            indentation: "  ".into(),
            line_width: 100,
            keyword_case: KeywordCase::Upper,
            clause_breaks: ClauseBreakPolicy::EveryClause,
            comma_after_break: true,
            result_item_breaks: ItemBreakPolicy::EveryItem,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    #[error("query parse error: {0}")]
    Parse(String),
    #[error("unsupported AST variant: {0}")]
    Unsupported(String),
    #[error("invalid format options: {0}")]
    InvalidOptions(String),
}

pub fn format_query(input: &str, options: &FormatOptions) -> Result<String, FormatError> {
    validate_options(options)?;
    let program = parser::parse(input).map_err(|e| FormatError::Parse(e.to_string()))?;
    format_program(&program, options)
}

pub fn format_program(
    program: &GqlProgram,
    options: &FormatOptions,
) -> Result<String, FormatError> {
    validate_options(options)?;
    let mut f = Formatter { options };
    f.program(program)
}

fn validate_options(options: &FormatOptions) -> Result<(), FormatError> {
    if options.indentation.is_empty() {
        return Err(FormatError::InvalidOptions(
            "indentation must not be empty".into(),
        ));
    }
    if options.line_width == 0 {
        return Err(FormatError::InvalidOptions(
            "line_width must be positive".into(),
        ));
    }
    Ok(())
}

struct Formatter<'a> {
    options: &'a FormatOptions,
}

struct ResultClauses<'a> {
    group: &'a Option<GroupByClause>,
    having: &'a Option<Expr>,
    order: &'a Option<OrderByClause>,
    limit: &'a Option<LimitClause>,
    offset: &'a Option<OffsetClause>,
}

impl<'a> Formatter<'a> {
    fn kw(&self, s: &str) -> String {
        match self.options.keyword_case {
            KeywordCase::Upper => s.to_ascii_uppercase(),
            KeywordCase::Lower => s.to_ascii_lowercase(),
        }
    }
    fn unsupported<T>(&self, name: &str) -> Result<T, FormatError> {
        Err(FormatError::Unsupported(name.into()))
    }
    fn program(&mut self, p: &GqlProgram) -> Result<String, FormatError> {
        if !p.session_activity.is_empty() {
            return self.unsupported("session activity");
        }
        let tx = p
            .transaction_activity
            .as_ref()
            .ok_or_else(|| FormatError::Unsupported("empty program".into()))?;
        if tx.start.is_some() || tx.end.is_some() {
            return self.unsupported("transaction wrapper");
        }
        let body = tx
            .body
            .as_ref()
            .ok_or_else(|| FormatError::Unsupported("empty statement block".into()))?;
        self.statement_block(body)
    }
    fn statement_block(&mut self, b: &StatementBlock) -> Result<String, FormatError> {
        let mut out = self.statement(&b.first)?;
        for next in &b.next {
            if next.yield_items.is_some() {
                return self.unsupported("NEXT YIELD");
            }
            out.push('\n');
            out.push_str(&self.kw("NEXT"));
            out.push(' ');
            out.push_str(&self.statement(&next.statement)?);
        }
        Ok(out)
    }
    fn statement(&mut self, s: &Statement) -> Result<String, FormatError> {
        match s {
            Statement::Query(q) => self.composite(q),
            _ => self.unsupported("statement"),
        }
    }
    fn composite(&mut self, q: &CompositeQueryExpr) -> Result<String, FormatError> {
        let mut out = self.linear(&q.left)?;
        for (op, rhs) in &q.rest {
            out.push('\n');
            out.push_str(&self.set_op(*op));
            out.push('\n');
            out.push_str(&self.linear(rhs)?);
        }
        Ok(out)
    }
    fn set_op(&self, op: SetOp) -> String {
        match op {
            SetOp::Union => self.kw("UNION"),
            SetOp::UnionAll => format!("{} ALL", self.kw("UNION")),
            SetOp::UnionDistinct => format!("{} DISTINCT", self.kw("UNION")),
            SetOp::Except => self.kw("EXCEPT"),
            SetOp::ExceptAll => format!("{} ALL", self.kw("EXCEPT")),
            SetOp::ExceptDistinct => format!("{} DISTINCT", self.kw("EXCEPT")),
            SetOp::Intersect => self.kw("INTERSECT"),
            SetOp::IntersectAll => format!("{} ALL", self.kw("INTERSECT")),
            SetOp::IntersectDistinct => format!("{} DISTINCT", self.kw("INTERSECT")),
            SetOp::Otherwise => self.kw("OTHERWISE"),
        }
    }
    fn linear(&mut self, q: &LinearQueryStatement) -> Result<String, FormatError> {
        if q.at_schema.is_some() || !q.prefix_bindings.is_empty() {
            return self.unsupported("linear-query prefix");
        }
        let mut lines = Vec::new();
        for part in &q.parts {
            lines.push(self.part(part)?);
        }
        if let Some(result) = &q.result {
            lines.extend(self.result(result)?);
        }
        if lines.is_empty() {
            return self.unsupported("empty linear query");
        }
        Ok(self.render_lines(&lines))
    }
    fn render_lines(&self, lines: &[String]) -> String {
        match self.options.clause_breaks {
            ClauseBreakPolicy::EveryClause => lines.join("\n"),
            ClauseBreakPolicy::Compact => {
                let mut out = String::new();
                let mut current_width = 0;
                for line in lines {
                    let raw_block = line.trim_end();
                    let compact_block = raw_block.trim_start();
                    if compact_block.is_empty() {
                        continue;
                    }
                    if compact_block.contains('\n') {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(compact_block);
                        current_width = compact_block.lines().last().map_or(0, str::len);
                        continue;
                    }
                    let separator_width = usize::from(current_width > 0);
                    if current_width > 0
                        && current_width + separator_width + compact_block.len()
                            > self.options.line_width
                    {
                        out.push('\n');
                        current_width = 0;
                    }
                    let block = if current_width == 0
                        && out.ends_with('\n')
                        && raw_block.len() != compact_block.len()
                    {
                        raw_block
                    } else {
                        compact_block
                    };
                    if current_width > 0 {
                        out.push(' ');
                        current_width += 1;
                    }
                    out.push_str(block);
                    current_width += block.len();
                }
                out
            }
        }
    }
    fn part(&mut self, p: &SimpleQueryStatement) -> Result<String, FormatError> {
        match p {
            SimpleQueryStatement::Match(m) => {
                if m.yield_items.is_some() {
                    return self.unsupported("MATCH YIELD");
                }
                let prefix = if m.optional {
                    format!("{} {}", self.kw("OPTIONAL"), self.kw("MATCH"))
                } else {
                    self.kw("MATCH")
                };
                if m.graph_name.is_none()
                    && let Some(where_clause) = &m.pattern.where_clause
                {
                    let paths = self.pattern_paths(&m.pattern)?;
                    let condition = format!("{} {}", self.kw("WHERE"), self.expr(where_clause)?);
                    let inline_len = prefix.len() + 1 + paths.len() + 1 + condition.len();
                    if inline_len > self.options.line_width {
                        return Ok(format!(
                            "{} {}\n{}{}",
                            prefix, paths, self.options.indentation, condition
                        ));
                    }
                }
                let mut s = format!("{} {}", prefix, self.pattern(&m.pattern)?);
                if let Some(g) = &m.graph_name {
                    s.push_str(&format!(" {} {}", self.kw("ON"), self.name(g)));
                }
                Ok(s)
            }
            SimpleQueryStatement::Filter(x) => Ok(format!(
                "{}{} {}",
                self.kw("FILTER"),
                if x.where_keyword {
                    format!(" {}", self.kw("WHERE"))
                } else {
                    String::new()
                },
                self.expr(&x.condition)?
            )),
            SimpleQueryStatement::Let(x) => {
                let vals = x
                    .bindings
                    .iter()
                    .map(|b| Ok(format!("{} = {}", b.variable, self.expr(&b.value)?)))
                    .collect::<Result<Vec<_>, FormatError>>()?;
                Ok(format!("{} {}", self.kw("LET"), vals.join(", ")))
            }
            SimpleQueryStatement::For(x) => Ok(format!(
                "{} {} {} {}{}",
                self.kw("FOR"),
                x.variable,
                self.kw("IN"),
                self.expr(&x.list)?,
                match &x.ordinality {
                    None => String::new(),
                    Some(_) => return self.unsupported("FOR ordinality"),
                }
            )),
            SimpleQueryStatement::OrderBy(x) => self.order(x),
            SimpleQueryStatement::Limit(x) => {
                Ok(format!("{} {}", self.kw("LIMIT"), self.expr(&x.count)?))
            }
            SimpleQueryStatement::Offset(x) => Ok(format!(
                "{} {}",
                if x.skip_keyword {
                    self.kw("SKIP")
                } else {
                    self.kw("OFFSET")
                },
                self.expr(&x.count)?
            )),
            #[cfg(feature = "gleaph")]
            SimpleQueryStatement::Search(x) => self.search(x),
            _ => self.unsupported("simple query statement"),
        }
    }
    fn result(&mut self, r: &ResultStatement) -> Result<Vec<String>, FormatError> {
        match r {
            ResultStatement::Return(x) => self.return_body(&x.set_quantifier, &x.body, "RETURN"),
            ResultStatement::Select(x) => {
                if x.source.is_some() {
                    return self.unsupported("SELECT source");
                }
                self.select_body(&x.set_quantifier, &x.body)
            }
            ResultStatement::Finish => Ok(vec![self.kw("FINISH")]),
        }
    }
    fn return_body(
        &mut self,
        q: &SetQuantifier,
        b: &ReturnBody,
        word: &str,
    ) -> Result<Vec<String>, FormatError> {
        match b {
            ReturnBody::Star => Ok(vec![format!("{}{} *", self.kw(word), self.quantifier(*q))]),
            ReturnBody::Items {
                items,
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => self.items(
                word,
                *q,
                items,
                ResultClauses {
                    group: group_by,
                    having,
                    order: order_by,
                    limit,
                    offset,
                },
            ),
            #[cfg(feature = "cypher")]
            ReturnBody::NoBindings => Ok(vec![format!("{} NO BINDINGS", self.kw(word))]),
        }
    }
    fn select_body(
        &mut self,
        q: &SetQuantifier,
        b: &SelectBody,
    ) -> Result<Vec<String>, FormatError> {
        match b {
            SelectBody::Star {
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => {
                let mut lines = vec![format!("{}{} *", self.kw("SELECT"), self.quantifier(*q))];
                self.append_result_clauses(
                    &mut lines,
                    ResultClauses {
                        group: group_by,
                        having,
                        order: order_by,
                        limit,
                        offset,
                    },
                )?;
                Ok(lines)
            }
            SelectBody::Items {
                items,
                group_by,
                having,
                order_by,
                limit,
                offset,
            } => self.items(
                "SELECT",
                *q,
                items,
                ResultClauses {
                    group: group_by,
                    having,
                    order: order_by,
                    limit,
                    offset,
                },
            ),
        }
    }
    fn quantifier(&self, q: SetQuantifier) -> String {
        match q {
            SetQuantifier::None => String::new(),
            SetQuantifier::All => format!(" {}", self.kw("ALL")),
            SetQuantifier::Distinct => format!(" {}", self.kw("DISTINCT")),
        }
    }
    fn items(
        &mut self,
        word: &str,
        q: SetQuantifier,
        items: &[ReturnItem],
        clauses: ResultClauses<'_>,
    ) -> Result<Vec<String>, FormatError> {
        let mut lines = Vec::new();
        let head = format!("{}{}", self.kw(word), self.quantifier(q));
        if items.is_empty() {
            lines.push(head);
        } else if self.options.result_item_breaks == ItemBreakPolicy::Compact {
            let items_text = self.items_text(items)?;
            if head.len() + 1 + items_text.len() <= self.options.line_width {
                lines.push(format!("{} {}", head, items_text));
            } else {
                lines.push(head);
                self.append_items(&mut lines, items)?;
            }
        } else {
            lines.push(head);
            self.append_items(&mut lines, items)?;
        }
        self.append_result_clauses(&mut lines, clauses)?;
        Ok(lines)
    }
    fn append_items(
        &mut self,
        lines: &mut Vec<String>,
        items: &[ReturnItem],
    ) -> Result<(), FormatError> {
        for (i, item) in items.iter().enumerate() {
            let comma = if i + 1 < items.len() { "," } else { "" };
            let item = self.item(item)?;
            lines.push(if self.options.comma_after_break {
                format!("{}{}{}", self.options.indentation, item, comma)
            } else if i == 0 {
                format!("{}{}", self.options.indentation, item)
            } else {
                format!(", {}{}", self.options.indentation, item)
            });
        }
        Ok(())
    }
    fn append_result_clauses(
        &mut self,
        lines: &mut Vec<String>,
        clauses: ResultClauses<'_>,
    ) -> Result<(), FormatError> {
        if let Some(g) = clauses.group {
            lines.push(format!(
                "{} {}",
                self.kw("GROUP BY"),
                g.items
                    .iter()
                    .map(|e| self.expr(e))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ")
            ));
        }
        if let Some(h) = clauses.having {
            lines.push(format!("{} {}", self.kw("HAVING"), self.expr(h)?));
        }
        if let Some(o) = clauses.order {
            lines.push(self.order(o)?);
        }
        if let Some(l) = clauses.limit {
            lines.push(format!("{} {}", self.kw("LIMIT"), self.expr(&l.count)?));
        }
        if let Some(o) = clauses.offset {
            lines.push(format!(
                "{} {}",
                if o.skip_keyword {
                    self.kw("SKIP")
                } else {
                    self.kw("OFFSET")
                },
                self.expr(&o.count)?
            ));
        }
        Ok(())
    }
    fn items_text(&mut self, items: &[ReturnItem]) -> Result<String, FormatError> {
        items
            .iter()
            .map(|i| self.item(i))
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.join(", "))
    }
    fn item(&mut self, i: &ReturnItem) -> Result<String, FormatError> {
        Ok(match &i.alias {
            Some(a) => format!("{} {} {}", self.expr(&i.expr)?, self.kw("AS"), a),
            None => self.expr(&i.expr)?,
        })
    }
    fn order(&mut self, o: &OrderByClause) -> Result<String, FormatError> {
        let mut v = Vec::new();
        for i in &o.items {
            let mut s = self.expr(&i.expr)?;
            if let Some(d) = i.direction {
                s.push(' ');
                s.push_str(match d {
                    SortDirection::Asc => "ASC",
                    SortDirection::Ascending => "ASCENDING",
                    SortDirection::Desc => "DESC",
                    SortDirection::Descending => "DESCENDING",
                });
            }
            if let Some(n) = i.null_order {
                s.push_str(if n == NullOrder::First {
                    " NULLS FIRST"
                } else {
                    " NULLS LAST"
                });
            }
            v.push(s);
        }
        Ok(format!("{} {}", self.kw("ORDER BY"), v.join(", ")))
    }
    fn pattern(&mut self, p: &GraphPattern) -> Result<String, FormatError> {
        let mut s = self.pattern_paths(p)?;
        if let Some(w) = &p.where_clause {
            s.push_str(&format!(" {} {}", self.kw("WHERE"), self.expr(w)?));
        }
        Ok(s)
    }
    fn pattern_paths(&mut self, p: &GraphPattern) -> Result<String, FormatError> {
        if p.match_mode.is_some() || p.keep.is_some() {
            return self.unsupported("graph pattern modifier");
        }
        Ok(p.paths
            .iter()
            .map(|x| self.path(x))
            .collect::<Result<Vec<_>, _>>()?
            .join(", "))
    }
    fn path(&mut self, p: &PathPattern) -> Result<String, FormatError> {
        if p.variable.is_some() || p.prefix.is_some() || !p.extensions.is_empty() {
            return self.unsupported("path pattern modifier");
        }
        match &p.expr {
            PathPatternExpr::Term(t) => t
                .factors
                .iter()
                .map(|f| self.factor(f))
                .collect::<Result<Vec<_>, _>>()
                .map(|v| v.join("")),
            _ => self.unsupported("path pattern expression"),
        }
    }
    fn factor(&mut self, f: &PathFactor) -> Result<String, FormatError> {
        let mut s = match &f.primary {
            PathPrimary::Node(n) => self.node(n),
            PathPrimary::Edge(e) => self.edge(e),
            _ => self.unsupported("path primary"),
        }?;
        if let Some(q) = &f.quantifier {
            s.push_str(match q {
                PathQuantifier::Star => "*",
                PathQuantifier::Plus => "+",
                PathQuantifier::Optional => "?",
                PathQuantifier::Fixed(n) => return Ok(format!("{}{{{n}}}", s)),
                PathQuantifier::Range { lower, upper } => {
                    return Ok(format!(
                        "{}{{{},{}}}",
                        s,
                        lower,
                        upper.map_or(String::new(), |n| n.to_string())
                    ));
                }
            });
        }
        Ok(s)
    }
    fn node(&mut self, n: &NodePattern) -> Result<String, FormatError> {
        let mut s = "(".to_string();
        if let Some(v) = &n.variable {
            s.push_str(v);
        }
        if let Some(l) = &n.label {
            s.push(':');
            s.push_str(&self.label(l));
        }
        if !n.properties.is_empty() {
            s.push_str(" {");
            s.push_str(
                &n.properties
                    .iter()
                    .map(|p| Ok(format!("{}: {}", p.name, self.expr(&p.value)?)))
                    .collect::<Result<Vec<_>, FormatError>>()?
                    .join(", "),
            );
            s.push('}');
        }
        if let Some(w) = &n.where_clause {
            s.push_str(&format!(" {} {}", self.kw("WHERE"), self.expr(w)?));
        }
        s.push(')');
        Ok(s)
    }
    fn edge(&mut self, e: &EdgePattern) -> Result<String, FormatError> {
        let body = {
            let mut x = String::new();
            if let Some(v) = &e.variable {
                x.push_str(v);
            }
            if let Some(l) = &e.label {
                x.push(':');
                x.push_str(&self.label(l));
            }
            if !e.properties.is_empty() {
                x.push_str(" {");
                x.push_str(
                    &e.properties
                        .iter()
                        .map(|p| Ok(format!("{}: {}", p.name, self.expr(&p.value)?)))
                        .collect::<Result<Vec<_>, FormatError>>()?
                        .join(", "),
                );
                x.push('}');
            }
            if let Some(w) = &e.where_clause {
                x.push_str(&format!(" {} {}", self.kw("WHERE"), self.expr(w)?));
            }
            x
        };
        Ok(match e.direction {
            EdgeDirection::PointingRight => format!("-[{}]->", body),
            EdgeDirection::PointingLeft => format!("<-[{}]-", body),
            EdgeDirection::LeftOrRight => format!("<-[{}]->", body),
            EdgeDirection::Undirected | EdgeDirection::AnyDirection => format!("-[{}]-", body),
            _ => return self.unsupported("edge direction"),
        })
    }
    fn label(&self, l: &LabelExpr) -> String {
        match l {
            LabelExpr::Name(s) => s.clone(),
            LabelExpr::Wildcard => "%".into(),
            LabelExpr::And(a, b) => format!("{}&{}", self.label(a), self.label(b)),
            LabelExpr::Or(a, b) => format!("{}|{}", self.label(a), self.label(b)),
            LabelExpr::Not(a) => format!("!{}", self.label(a)),
        }
    }
    fn name(&self, n: &ObjectName) -> String {
        n.parts.join(".")
    }
    fn expr(&mut self, e: &Expr) -> Result<String, FormatError> {
        self.expr_prec(e, 0)
    }
    fn expr_prec(&mut self, e: &Expr, parent: u8) -> Result<String, FormatError> {
        let (s, p) = match &e.kind {
            ExprKind::Literal(v) => (self.literal(v)?, 10),
            ExprKind::Variable(v) => (v.clone(), 10),
            ExprKind::Parameter(v) => (
                if v.starts_with('$') {
                    v.clone()
                } else {
                    format!("${v}")
                },
                10,
            ),
            ExprKind::PropertyAccess { expr, property } => {
                (format!("{}.{}", self.expr_prec(expr, 10)?, property), 10)
            }
            ExprKind::ElementId(x) => (format!("{}({})", self.kw("ELEMENT_ID"), self.expr(x)?), 10),
            ExprKind::FunctionCall {
                name,
                args,
                distinct,
            } => {
                let mut a = args
                    .iter()
                    .map(|x| self.expr(x))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ");
                if *distinct {
                    a = format!("{} {}", self.kw("DISTINCT"), a);
                }
                (format!("{}({})", self.name(name), a), 10)
            }
            ExprKind::Paren(x) => (format!("({})", self.expr(x)?), 10),
            ExprKind::UnaryOp { op, expr } => (
                format!(
                    "{}{}",
                    if *op == UnaryOp::Neg { "-" } else { "+" },
                    self.expr_prec(expr, 9)?
                ),
                9,
            ),
            ExprKind::Not(x) => (format!("{} {}", self.kw("NOT"), self.expr_prec(x, 3)?), 3),
            ExprKind::And(a, b) => (
                format!(
                    "{} {} {}",
                    self.expr_prec(a, 2)?,
                    self.kw("AND"),
                    self.expr_prec(b, 3)?
                ),
                2,
            ),
            ExprKind::Or(a, b) => (
                format!(
                    "{} {} {}",
                    self.expr_prec(a, 1)?,
                    self.kw("OR"),
                    self.expr_prec(b, 2)?
                ),
                1,
            ),
            ExprKind::Compare { left, op, right } => (
                format!(
                    "{} {} {}",
                    self.expr_prec(left, 3)?,
                    match op {
                        CmpOp::Eq => "=",
                        CmpOp::Ne => "<>",
                        CmpOp::Lt => "<",
                        CmpOp::Le => "<=",
                        CmpOp::Gt => ">",
                        CmpOp::Ge => ">=",
                    },
                    self.expr_prec(right, 4)?
                ),
                3,
            ),
            ExprKind::BinaryOp { left, op, right } => (
                format!(
                    "{} {} {}",
                    self.expr_prec(left, 4)?,
                    match op {
                        BinaryOp::Add => "+",
                        BinaryOp::Sub => "-",
                        BinaryOp::Mul => "*",
                        BinaryOp::Div => "/",
                    },
                    self.expr_prec(
                        right,
                        if matches!(op, BinaryOp::Mul | BinaryOp::Div) {
                            6
                        } else {
                            5
                        }
                    )?
                ),
                if matches!(op, BinaryOp::Mul | BinaryOp::Div) {
                    5
                } else {
                    4
                },
            ),
            ExprKind::IsNull(x) => (
                format!("{} {}", self.expr_prec(x, 4)?, self.kw("IS NULL")),
                4,
            ),
            ExprKind::IsNotNull(x) => (
                format!("{} {}", self.expr_prec(x, 4)?, self.kw("IS NOT NULL")),
                4,
            ),
            _ => return self.unsupported("expression"),
        };
        if p < parent {
            Ok(format!("({s})"))
        } else {
            Ok(s)
        }
    }
    fn literal(&self, v: &Value) -> Result<String, FormatError> {
        Ok(match v {
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Bool(b) => {
                if *b {
                    self.kw("TRUE")
                } else {
                    self.kw("FALSE")
                }
            }
            Value::Bytes(_)
            | Value::Extension(_)
            | Value::List(_)
            | Value::Path(_)
            | Value::Record(_) => return self.unsupported("literal"),
            _ => v.to_string(),
        })
    }
    #[cfg(feature = "gleaph")]
    fn search(&mut self, x: &SearchStatement) -> Result<String, FormatError> {
        let SearchProvider::VectorIndex(spec) = &x.provider;
        let SearchOutputBinding { kind, alias } = &x.output;
        let metric = match kind {
            SearchOutputKind::Score => "SCORE",
            SearchOutputKind::Distance => "DISTANCE",
        };
        let mut s = format!(
            "{} {} {} (\n{}{} {}\n{}{} {}\n",
            self.kw("SEARCH"),
            x.binding,
            self.kw("IN"),
            self.options.indentation,
            self.kw("VECTOR INDEX"),
            self.name(&spec.index_name),
            self.options.indentation,
            self.kw("FOR"),
            self.expr(&spec.query)?
        );
        if let Some(filter) = &spec.filter {
            s.push_str(&format!(
                "{}{} {}\n",
                self.options.indentation,
                self.kw("WHERE"),
                self.expr(filter)?
            ));
        }
        s.push_str(&format!(
            "{}{} {}\n{}) {} {}",
            self.options.indentation,
            self.kw("LIMIT"),
            self.expr(&spec.limit)?,
            self.options.indentation,
            self.kw(metric),
            self.kw("AS")
        ));
        s.push_str(&format!(" {}", alias));
        Ok(s)
    }
}

impl fmt::Display for KeywordCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}
