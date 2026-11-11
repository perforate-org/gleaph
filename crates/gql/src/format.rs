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
            #[cfg(feature = "gleaph")]
            Statement::ExplainAuthorization(x) => self.explain_authorization(x),
            _ => self.unsupported("statement"),
        }
    }
    #[cfg(feature = "gleaph")]
    fn explain_authorization(
        &mut self,
        x: &ExplainAuthorizationStatement,
    ) -> Result<String, FormatError> {
        let mut out = self.kw("EXPLAIN AUTHORIZATION");
        out.push('\n');
        out.push_str(&self.kw("FOR PREPARED QUERY"));
        out.push(' ');
        out.push_str(&x.query_name);
        if let Some(principal) = &x.by_principal {
            out.push('\n');
            out.push_str(&self.kw("BY PRINCIPAL"));
            out.push_str(&format!(" '{principal}'"));
        }
        Ok(out)
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
                    .map(|b| {
                        Ok(format!(
                            "{} = {}",
                            self.ident(&b.variable),
                            self.expr(&b.value)?
                        ))
                    })
                    .collect::<Result<Vec<_>, FormatError>>()?;
                Ok(format!("{} {}", self.kw("LET"), vals.join(", ")))
            }
            SimpleQueryStatement::For(x) => Ok(format!(
                "{} {} {} {}{}",
                self.kw("FOR"),
                self.ident(&x.variable),
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
            #[cfg(feature = "gleaph")]
            SimpleQueryStatement::Grant(x) => {
                self.authorization_change("GRANT", &x.target, &x.subject, "TO")
            }
            #[cfg(feature = "gleaph")]
            SimpleQueryStatement::Revoke(x) => {
                self.authorization_change("REVOKE", &x.target, &x.subject, "FROM")
            }
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
            Some(a) => format!(
                "{} {} {}",
                self.expr(&i.expr)?,
                self.kw("AS"),
                self.ident(a)
            ),
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
            PathPrimary::Simplified(p) => self.simplified_path(p),
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
    fn simplified_path(&mut self, p: &SimplifiedPathPattern) -> Result<String, FormatError> {
        p.elements
            .iter()
            .map(|element| {
                let (opening, closing) = match element.direction {
                    EdgeDirection::PointingRight => ("-/", "/->"),
                    EdgeDirection::PointingLeft => ("<-/", "/-"),
                    EdgeDirection::LeftOrRight => ("<-/", "/->"),
                    EdgeDirection::Undirected => ("~/", "/~"),
                    EdgeDirection::LeftOrUndirected => ("<~/", "/~"),
                    EdgeDirection::UndirectedOrRight => ("~/", "/~>"),
                    EdgeDirection::AnyDirection => ("-/", "/-"),
                };
                Ok(format!(
                    "{}{}{}",
                    opening,
                    self.simplified_contents(&element.contents)?,
                    closing
                ))
            })
            .collect::<Result<Vec<_>, FormatError>>()
            .map(|elements| elements.join(""))
    }
    fn simplified_contents(
        &mut self,
        contents: &SimplifiedContents,
    ) -> Result<String, FormatError> {
        match contents {
            SimplifiedContents::Label(label) => Ok(self.label(label)),
            SimplifiedContents::Negation(inner) => {
                Ok(format!("!{}", self.simplified_contents(inner)?))
            }
            SimplifiedContents::Conjunction(left, right) => Ok(format!(
                "({}&{})",
                self.simplified_contents(left)?,
                self.simplified_contents(right)?
            )),
            SimplifiedContents::Union(left, right) => Ok(format!(
                "({}|{})",
                self.simplified_contents(left)?,
                self.simplified_contents(right)?
            )),
            SimplifiedContents::MultisetAlternation(left, right) => Ok(format!(
                "({}|+|{})",
                self.simplified_contents(left)?,
                self.simplified_contents(right)?
            )),
            SimplifiedContents::Concatenation(left, right) => Ok(format!(
                "({} {})",
                self.simplified_contents(left)?,
                self.simplified_contents(right)?
            )),
            SimplifiedContents::Quantified(inner, quantifier) => Ok(format!(
                "{}{}",
                self.simplified_contents(inner)?,
                self.path_quantifier(quantifier)?
            )),
            SimplifiedContents::DirectionOverride(direction, inner) => {
                let value = self.simplified_contents(inner)?;
                Ok(match direction {
                    EdgeDirection::PointingRight => format!("{}>", value),
                    EdgeDirection::PointingLeft => format!("<{}", value),
                    EdgeDirection::LeftOrRight => format!("<{}>", value),
                    EdgeDirection::Undirected => format!("~{}", value),
                    EdgeDirection::LeftOrUndirected => format!("<~{}", value),
                    EdgeDirection::UndirectedOrRight => format!("~{}>", value),
                    EdgeDirection::AnyDirection => format!("-{}", value),
                })
            }
            SimplifiedContents::Group(inner) => {
                Ok(format!("({})", self.simplified_contents(inner)?))
            }
        }
    }
    fn path_quantifier(&self, quantifier: &PathQuantifier) -> Result<String, FormatError> {
        Ok(match quantifier {
            PathQuantifier::Star => "*".into(),
            PathQuantifier::Plus => "+".into(),
            PathQuantifier::Optional => "?".into(),
            PathQuantifier::Fixed(n) => format!("{{{n}}}"),
            PathQuantifier::Range { lower, upper } => {
                format!(
                    "{{{lower},{}}}",
                    upper.map_or(String::new(), |n| n.to_string())
                )
            }
        })
    }
    fn node(&mut self, n: &NodePattern) -> Result<String, FormatError> {
        let mut s = "(".to_string();
        if let Some(v) = &n.variable {
            s.push_str(&self.ident(v));
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
                    .map(|p| Ok(format!("{}: {}", self.ident(&p.name), self.expr(&p.value)?)))
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
                x.push_str(&self.ident(v));
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
                        .map(|p| Ok(format!("{}: {}", self.ident(&p.name), self.expr(&p.value)?)))
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
            EdgeDirection::Undirected => format!("~[{}]~", body),
            EdgeDirection::LeftOrUndirected => format!("<~[{}]~", body),
            EdgeDirection::UndirectedOrRight => format!("~[{}]~>", body),
            EdgeDirection::AnyDirection => format!("-[{}]-", body),
        })
    }
    fn label(&self, l: &LabelExpr) -> String {
        match l {
            LabelExpr::Name(s) => self.ident(s),
            LabelExpr::Wildcard => "%".into(),
            LabelExpr::And(a, b) => format!("{}&{}", self.label(a), self.label(b)),
            LabelExpr::Or(a, b) => format!("{}|{}", self.label(a), self.label(b)),
            LabelExpr::Not(a) => format!("!{}", self.label(a)),
        }
    }
    fn name(&self, n: &ObjectName) -> String {
        n.parts
            .iter()
            .map(|part| self.ident(part))
            .collect::<Vec<_>>()
            .join(".")
    }
    /// Renders `s` as an identifier, quoting it when the unquoted spelling would parse as
    /// something else: a non-identifier character sequence, or a reserved keyword.
    fn ident(&self, s: &str) -> String {
        if is_plain_identifier(s) && !gleaph_gql_keywords::is_reserved_keyword(s) {
            s.to_string()
        } else {
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\"\""))
        }
    }
    fn expr(&mut self, e: &Expr) -> Result<String, FormatError> {
        self.expr_prec(e, 0)
    }
    fn expr_prec(&mut self, e: &Expr, parent: u8) -> Result<String, FormatError> {
        let (s, p) = match &e.kind {
            ExprKind::Literal(v) => (self.literal(v)?, 10),
            ExprKind::Variable(v) => (self.ident(v), 10),
            ExprKind::Parameter(v) => (
                if v.starts_with('$') {
                    v.clone()
                } else {
                    format!("${v}")
                },
                10,
            ),
            ExprKind::PropertyAccess { expr, property } => (
                format!("{}.{}", self.expr_prec(expr, 10)?, self.ident(property)),
                10,
            ),
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
            ExprKind::Xor(a, b) => (
                format!(
                    "{} {} {}",
                    self.expr_prec(a, 1)?,
                    self.kw("XOR"),
                    self.expr_prec(b, 2)?
                ),
                1,
            ),
            ExprKind::InList {
                expr,
                list,
                negated,
            } => (
                format!(
                    "{}{} {} ({})",
                    self.expr_prec(expr, 4)?,
                    if *negated {
                        format!(" {}", self.kw("NOT"))
                    } else {
                        String::new()
                    },
                    self.kw("IN"),
                    self.expr_list(list)?
                ),
                4,
            ),
            ExprKind::StringPredicate {
                expr,
                kind,
                pattern,
                negated,
                escape,
            } => {
                // `StringPredicateKind` is an empty enum when the `cypher` dialect is off, so the
                // whole rendering is gated rather than matched.
                #[cfg(not(feature = "cypher"))]
                {
                    let _ = (expr, kind, pattern, negated, escape);
                    return self.unsupported("string predicate");
                }
                #[cfg(feature = "cypher")]
                {
                    let keyword = match kind {
                        StringPredicateKind::StartsWith => {
                            format!("{} {}", self.kw("STARTS"), self.kw("WITH"))
                        }
                        StringPredicateKind::EndsWith => {
                            format!("{} {}", self.kw("ENDS"), self.kw("WITH"))
                        }
                        StringPredicateKind::Contains => self.kw("CONTAINS"),
                        StringPredicateKind::Like => self.kw("LIKE"),
                        StringPredicateKind::ILike => self.kw("ILIKE"),
                    };
                    let mut text = format!(
                        "{}{} {} {}",
                        self.expr_prec(expr, 4)?,
                        if *negated {
                            format!(" {}", self.kw("NOT"))
                        } else {
                            String::new()
                        },
                        keyword,
                        self.expr_prec(pattern, 4)?
                    );
                    if let Some(escape) = escape {
                        text.push_str(&format!(" {} {}", self.kw("ESCAPE"), self.expr(escape)?));
                    }
                    (text, 4)
                }
            }
            ExprKind::IsNormalized {
                expr,
                form,
                negated,
            } => (
                format!(
                    "{} {} {}{}",
                    self.expr_prec(expr, 4)?,
                    self.kw("IS"),
                    if *negated {
                        format!("{} ", self.kw("NOT"))
                    } else {
                        String::new()
                    },
                    match form {
                        NormalForm::NFC => format!("{} {}", self.kw("NFC"), self.kw("NORMALIZED")),
                        NormalForm::NFD => format!("{} {}", self.kw("NFD"), self.kw("NORMALIZED")),
                        NormalForm::NFKC =>
                            format!("{} {}", self.kw("NFKC"), self.kw("NORMALIZED")),
                        NormalForm::NFKD =>
                            format!("{} {}", self.kw("NFKD"), self.kw("NORMALIZED")),
                    }
                ),
                4,
            ),
            ExprKind::IsTruth {
                expr,
                value,
                negated,
            } => (
                format!(
                    "{} {} {}{}",
                    self.expr_prec(expr, 4)?,
                    self.kw("IS"),
                    if *negated {
                        format!("{} ", self.kw("NOT"))
                    } else {
                        String::new()
                    },
                    match value {
                        TruthValue::True => self.kw("TRUE"),
                        TruthValue::False => self.kw("FALSE"),
                        TruthValue::Unknown => self.kw("UNKNOWN"),
                    }
                ),
                4,
            ),
            ExprKind::IsLabeled {
                expr,
                label,
                negated,
            } => (
                format!(
                    "{} {} {}{} {}",
                    self.expr_prec(expr, 4)?,
                    self.kw("IS"),
                    if *negated {
                        format!("{} ", self.kw("NOT"))
                    } else {
                        String::new()
                    },
                    self.kw("LABELED"),
                    self.label(label)
                ),
                4,
            ),
            ExprKind::IsSourceOf {
                node,
                edge,
                negated,
            }
            | ExprKind::IsDestOf {
                node,
                edge,
                negated,
            } => {
                let direction = if matches!(e.kind, ExprKind::IsSourceOf { .. }) {
                    "SOURCE"
                } else {
                    "DESTINATION"
                };
                (
                    format!(
                        "{} {} {}{} {}",
                        self.expr_prec(node, 4)?,
                        self.kw("IS"),
                        if *negated {
                            format!("{} ", self.kw("NOT"))
                        } else {
                            String::new()
                        },
                        self.kw(&format!("{direction} OF")),
                        self.expr_prec(edge, 4)?
                    ),
                    4,
                )
            }
            ExprKind::IsDirected { expr, negated } => (
                format!(
                    "{} {} {}{}",
                    self.expr_prec(expr, 4)?,
                    self.kw("IS"),
                    if *negated {
                        format!("{} ", self.kw("NOT"))
                    } else {
                        String::new()
                    },
                    self.kw("DIRECTED")
                ),
                4,
            ),
            ExprKind::AllDifferent(items) => (
                format!("{}({})", self.kw("ALL_DIFFERENT"), self.expr_list(items)?),
                10,
            ),
            ExprKind::Same(items) => (
                format!("{}({})", self.kw("SAME"), self.expr_list(items)?),
                10,
            ),
            ExprKind::PropertyExists { expr, property } => (
                format!(
                    "{}({}, {})",
                    self.kw("PROPERTY_EXISTS"),
                    self.expr_prec(expr, 4)?,
                    self.ident(property)
                ),
                10,
            ),
            ExprKind::ExistsPattern(pattern) => (
                format!("{} {{ {} }}", self.kw("EXISTS"), self.pattern(pattern)?),
                10,
            ),
            ExprKind::Coalesce(items) => (
                format!("{}({})", self.kw("COALESCE"), self.expr_list(items)?),
                10,
            ),
            ExprKind::NullIf(left, right) => (
                format!(
                    "{}({}, {})",
                    self.kw("NULLIF"),
                    self.expr(left)?,
                    self.expr(right)?
                ),
                10,
            ),
            ExprKind::ListLiteral(items) => (format!("[{}]", self.expr_list(items)?), 10),
            ExprKind::ListConstructor { keyword, items } => (
                format!("{}{}", self.kw(&keyword.0), self.bracket_list(items)?),
                10,
            ),
            ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
                (format!("{{{}}}", self.record_fields(fields)?), 10)
            }
            ExprKind::PathConstructor { elements } => (
                format!("{} [{}]", self.kw("PATH"), self.expr_list(elements)?),
                10,
            ),
            ExprKind::IsNull(x) => (
                format!("{} {}", self.expr_prec(x, 4)?, self.kw("IS NULL")),
                4,
            ),
            ExprKind::IsNotNull(x) => (
                format!("{} {}", self.expr_prec(x, 4)?, self.kw("IS NOT NULL")),
                4,
            ),
            ExprKind::Cast { expr, target } => (
                format!(
                    "{}({} {})",
                    self.kw("CAST"),
                    self.expr(expr)?,
                    self.value_type_with_as(target)?
                ),
                10,
            ),
            ExprKind::IsTyped {
                expr,
                target,
                negated,
            } => (
                format!(
                    "{} {} {}{} {}",
                    self.expr_prec(expr, 4)?,
                    self.kw("IS"),
                    if *negated {
                        format!("{} ", self.kw("NOT"))
                    } else {
                        String::new()
                    },
                    self.kw("TYPED"),
                    self.value_type(target)?
                ),
                4,
            ),
            ExprKind::Aggregate {
                func,
                expr,
                expr2,
                distinct,
                order_by,
                filter,
            } => {
                if order_by.is_some() || filter.is_some() {
                    return self.unsupported("aggregate ORDER BY / FILTER");
                }
                let name = match func {
                    AggregateFunc::Count | AggregateFunc::CountStar => self.kw("COUNT"),
                    AggregateFunc::Sum => self.kw("SUM"),
                    AggregateFunc::Avg => self.kw("AVG"),
                    AggregateFunc::Min => self.kw("MIN"),
                    AggregateFunc::Max => self.kw("MAX"),
                    AggregateFunc::Collect => self.kw("COLLECT_LIST"),
                    AggregateFunc::StddevSamp => self.kw("STDDEV_SAMP"),
                    AggregateFunc::StddevPop => self.kw("STDDEV_POP"),
                    AggregateFunc::PercentileCont => self.kw("PERCENTILE_CONT"),
                    AggregateFunc::PercentileDisc => self.kw("PERCENTILE_DISC"),
                };
                if matches!(func, AggregateFunc::CountStar) || expr.is_none() {
                    return Ok(format!("{name}(*)"));
                }
                let mut args = Vec::new();
                let first = self.expr(expr.as_ref().expect("checked above"))?;
                args.push(if *distinct {
                    format!("{} {}", self.kw("DISTINCT"), first)
                } else {
                    first
                });
                if let Some(second) = expr2 {
                    args.push(self.expr(second)?);
                }
                (format!("{name}({})", args.join(", ")), 10)
            }
            ExprKind::CaseSimple {
                operand,
                when_clauses,
                else_clause,
            } => {
                let mut text = format!("{} {}", self.kw("CASE"), self.expr(operand)?);
                for clause in when_clauses {
                    text.push_str(&format!(
                        " {} {} {} {}",
                        self.kw("WHEN"),
                        self.expr(&clause.condition)?,
                        self.kw("THEN"),
                        self.expr(&clause.result)?
                    ));
                }
                if let Some(else_clause) = else_clause {
                    text.push_str(&format!(" {} {}", self.kw("ELSE"), self.expr(else_clause)?));
                }
                text.push_str(&format!(" {}", self.kw("END")));
                (text, 10)
            }
            ExprKind::CaseSearched {
                when_clauses,
                else_clause,
            } => {
                let mut text = self.kw("CASE");
                for clause in when_clauses {
                    text.push_str(&format!(
                        " {} {} {} {}",
                        self.kw("WHEN"),
                        self.expr(&clause.condition)?,
                        self.kw("THEN"),
                        self.expr(&clause.result)?
                    ));
                }
                if let Some(else_clause) = else_clause {
                    text.push_str(&format!(" {} {}", self.kw("ELSE"), self.expr(else_clause)?));
                }
                text.push_str(&format!(" {}", self.kw("END")));
                (text, 10)
            }
            ExprKind::ExistsSubquery(query) => (
                format!("{} {{ {} }}", self.kw("EXISTS"), self.composite(query)?),
                10,
            ),
            ExprKind::ValueSubquery(query) => (
                format!("{} {{ {} }}", self.kw("VALUE"), self.composite(query)?),
                10,
            ),
            ExprKind::LetIn { bindings, expr } => {
                let definitions = bindings
                    .iter()
                    .map(|b| {
                        Ok(format!(
                            "{} = {}",
                            self.ident(&b.variable),
                            self.expr(&b.value)?
                        ))
                    })
                    .collect::<Result<Vec<_>, FormatError>>()?
                    .join(", ");
                (
                    format!(
                        "{} {} {} {} {}",
                        self.kw("LET"),
                        definitions,
                        self.kw("IN"),
                        self.expr(expr)?,
                        self.kw("END")
                    ),
                    10,
                )
            }
            ExprKind::PathLength(x) => (self.unary_call("PATH_LENGTH", x)?, 10),
            ExprKind::Elements(x) => (self.unary_call("ELEMENTS", x)?, 10),
            ExprKind::SessionUser => (self.kw("SESSION_USER"), 10),
            ExprKind::CurrentDate => (self.kw("CURRENT_DATE"), 10),
            ExprKind::CurrentTime => (self.kw("CURRENT_TIME"), 10),
            ExprKind::CurrentTimestamp => (self.kw("CURRENT_TIMESTAMP"), 10),
            ExprKind::CurrentLocalTime => (self.kw("LOCAL_TIME"), 10),
            ExprKind::CurrentLocalTimestamp => (self.kw("LOCAL_TIMESTAMP"), 10),
            ExprKind::DateLiteral(args) => (self.typed_literal("DATE", args)?, 10),
            ExprKind::TimeLiteral(args) => (self.typed_literal("TIME", args)?, 10),
            ExprKind::DatetimeLiteral(args) => (self.typed_literal("DATETIME", args)?, 10),
            ExprKind::TimestampLiteral(args) => (self.typed_literal("TIMESTAMP", args)?, 10),
            ExprKind::DurationLiteral(args) => (self.typed_literal("DURATION", args)?, 10),
            ExprKind::DateFunction(args) => (self.call("DATE", args)?, 10),
            ExprKind::TimeFunction(args) => (self.call("TIME", args)?, 10),
            ExprKind::ZonedTimeFunction(args) => (self.call("ZONED_TIME", args)?, 10),
            ExprKind::ZonedDatetimeFunction(args) => (self.call("ZONED_DATETIME", args)?, 10),
            ExprKind::LocalTimeFunction(args) => (self.call("LOCAL_TIME", args)?, 10),
            ExprKind::LocalDatetimeFunction(args) => (self.call("LOCAL_DATETIME", args)?, 10),
            ExprKind::DurationFunction(args) => (self.call("DURATION", args)?, 10),
            ExprKind::DurationBetween {
                left,
                right,
                qualifier,
            } => {
                let mut text = self.binary_call("DURATION_BETWEEN", left, right)?;
                if let Some(qualifier) = qualifier {
                    text.push_str(&match qualifier {
                        DurationQualifier::YearToMonth => {
                            format!(" {} {}", self.kw("YEAR TO"), self.kw("MONTH"))
                        }
                        DurationQualifier::DayToSecond => {
                            format!(" {} {}", self.kw("DAY TO"), self.kw("SECOND"))
                        }
                    });
                }
                (text, 10)
            }
            ExprKind::Normalize { expr, form } => (
                format!(
                    "{}({}, {})",
                    self.kw("NORMALIZE"),
                    self.expr(expr)?,
                    match form {
                        NormalForm::NFC => self.kw("NFC"),
                        NormalForm::NFD => self.kw("NFD"),
                        NormalForm::NFKC => self.kw("NFKC"),
                        NormalForm::NFKD => self.kw("NFKD"),
                    }
                ),
                10,
            ),
            ExprKind::Trim {
                spec,
                trim_char,
                expr,
            } => {
                let mut inner = String::new();
                if let Some(spec) = spec {
                    inner.push_str(&match spec {
                        TrimSpec::Leading => self.kw("LEADING"),
                        TrimSpec::Trailing => self.kw("TRAILING"),
                        TrimSpec::Both => self.kw("BOTH"),
                    });
                    inner.push(' ');
                }
                if let Some(trim_char) = trim_char {
                    inner.push_str(&self.expr(trim_char)?);
                    inner.push(' ');
                }
                if spec.is_some() || trim_char.is_some() {
                    inner.push_str(&format!("{} ", self.kw("FROM")));
                }
                inner.push_str(&self.expr(expr)?);
                (format!("{}({})", self.kw("TRIM"), inner), 10)
            }
            ExprKind::TrimList { list, count } => (self.binary_call("TRIM", list, count)?, 10),
            ExprKind::FoldString { kind, expr, chars } => {
                let name = match kind {
                    StringFoldKind::BTrim => self.kw("BTRIM"),
                    StringFoldKind::LTrim => self.kw("LTRIM"),
                    StringFoldKind::RTrim => self.kw("RTRIM"),
                };
                let mut args = vec![self.expr(expr)?];
                if let Some(chars) = chars {
                    args.push(self.expr(chars)?);
                }
                (format!("{name}({})", args.join(", ")), 10)
            }
            ExprKind::CharLength { keyword, expr } => {
                (format!("{}({})", self.kw(&keyword.0), self.expr(expr)?), 10)
            }
            ExprKind::ByteLength { keyword, expr } => {
                (format!("{}({})", self.kw(&keyword.0), self.expr(expr)?), 10)
            }
            ExprKind::Cardinality { keyword, expr } => {
                (format!("{}({})", self.kw(&keyword.0), self.expr(expr)?), 10)
            }
            ExprKind::Upper(x) => (self.unary_call("UPPER", x)?, 10),
            ExprKind::Lower(x) => (self.unary_call("LOWER", x)?, 10),
            ExprKind::Abs(x) => (self.unary_call("ABS", x)?, 10),
            ExprKind::Floor(x) => (self.unary_call("FLOOR", x)?, 10),
            ExprKind::Ceil(x) => (self.unary_call("CEIL", x)?, 10),
            ExprKind::Sqrt(x) => (self.unary_call("SQRT", x)?, 10),
            ExprKind::Exp(x) => (self.unary_call("EXP", x)?, 10),
            ExprKind::Ln(x) => (self.unary_call("LN", x)?, 10),
            ExprKind::Log10(x) => (self.unary_call("LOG10", x)?, 10),
            ExprKind::Sin(x) => (self.unary_call("SIN", x)?, 10),
            ExprKind::Cos(x) => (self.unary_call("COS", x)?, 10),
            ExprKind::Tan(x) => (self.unary_call("TAN", x)?, 10),
            ExprKind::Asin(x) => (self.unary_call("ASIN", x)?, 10),
            ExprKind::Acos(x) => (self.unary_call("ACOS", x)?, 10),
            ExprKind::Atan(x) => (self.unary_call("ATAN", x)?, 10),
            ExprKind::Degrees(x) => (self.unary_call("DEGREES", x)?, 10),
            ExprKind::Radians(x) => (self.unary_call("RADIANS", x)?, 10),
            ExprKind::Cot(x) => (self.unary_call("COT", x)?, 10),
            ExprKind::Sinh(x) => (self.unary_call("SINH", x)?, 10),
            ExprKind::Cosh(x) => (self.unary_call("COSH", x)?, 10),
            ExprKind::Tanh(x) => (self.unary_call("TANH", x)?, 10),
            ExprKind::Left(a, b) => (self.binary_call("LEFT", a, b)?, 10),
            ExprKind::Right(a, b) => (self.binary_call("RIGHT", a, b)?, 10),
            ExprKind::Mod(a, b) => (self.binary_call("MOD", a, b)?, 10),
            ExprKind::Log(a, b) => (self.binary_call("LOG", a, b)?, 10),
            ExprKind::Power(a, b) => (self.binary_call("POWER", a, b)?, 10),
            #[cfg(feature = "sql-compat")]
            ExprKind::Atan2(a, b) => (self.binary_call("ATAN2", a, b)?, 10),
            #[cfg(feature = "sql-compat")]
            ExprKind::Sign(x) => (self.unary_call("SIGN", x)?, 10),
            #[cfg(feature = "sql-compat")]
            ExprKind::Truncate { expr, places } => (
                self.call_with_optional_arg("TRUNCATE", expr, places.as_deref())?,
                10,
            ),
            #[cfg(feature = "sql-compat")]
            ExprKind::Round { expr, places } => (
                self.call_with_optional_arg("ROUND", expr, places.as_deref())?,
                10,
            ),
            #[cfg(feature = "cypher")]
            ExprKind::Nodes(x) => (self.unary_call("NODES", x)?, 10),
            #[cfg(feature = "cypher")]
            ExprKind::Edges(x) => (self.unary_call("EDGES", x)?, 10),
            #[cfg(feature = "cypher")]
            ExprKind::Labels(x) => (self.unary_call("LABELS", x)?, 10),
            #[cfg(feature = "cypher")]
            ExprKind::Label(x) => (self.unary_call("LABEL", x)?, 10),
            #[cfg(feature = "cypher")]
            ExprKind::Source(x) => (self.unary_call("SOURCE", x)?, 10),
            #[cfg(feature = "cypher")]
            ExprKind::Destination(x) => (self.unary_call("DESTINATION", x)?, 10),
            _ => return self.unsupported("expression"),
        };
        if p < parent {
            Ok(format!("({s})"))
        } else {
            Ok(s)
        }
    }
    /// Renders `NAME(expr)` for a one-argument function.
    fn unary_call(&mut self, name: &str, arg: &Expr) -> Result<String, FormatError> {
        Ok(format!("{}({})", self.kw(name), self.expr(arg)?))
    }

    /// Renders `NAME(a, b)` for a two-argument function.
    fn binary_call(&mut self, name: &str, a: &Expr, b: &Expr) -> Result<String, FormatError> {
        Ok(format!(
            "{}({}, {})",
            self.kw(name),
            self.expr(a)?,
            self.expr(b)?
        ))
    }

    /// Renders `NAME(args...)` for a function whose arguments are already expressions.
    fn call(&mut self, name: &str, args: &[Expr]) -> Result<String, FormatError> {
        Ok(format!("{}({})", self.kw(name), self.expr_list(args)?))
    }

    /// Renders `NAME(expr[, optional])` for the sql-compat optional-second-argument functions.
    #[cfg(feature = "sql-compat")]
    fn call_with_optional_arg(
        &mut self,
        name: &str,
        expr: &Expr,
        optional: Option<&Expr>,
    ) -> Result<String, FormatError> {
        match optional {
            Some(second) => self.binary_call(name, expr, second),
            None => self.unary_call(name, expr),
        }
    }

    /// Renders a typed temporal literal such as `DATE '2024-01-01'`.
    ///
    /// The parser stores exactly one text literal per typed literal; any other shape is an
    /// unknown AST form and is reported rather than rendered with invented text.
    fn typed_literal(&mut self, name: &str, args: &[Expr]) -> Result<String, FormatError> {
        let [arg] = args else {
            return self.unsupported("temporal literal shape");
        };
        if !matches!(arg.kind, ExprKind::Literal(Value::Text(_))) {
            return self.unsupported("temporal literal shape");
        }
        Ok(format!("{} {}", self.kw(name), self.expr(arg)?))
    }

    /// Renders a comma-separated expression list (may be empty).
    fn expr_list(&mut self, items: &[Expr]) -> Result<String, FormatError> {
        items
            .iter()
            .map(|item| self.expr(item))
            .collect::<Result<Vec<_>, _>>()
            .map(|rendered| rendered.join(", "))
    }

    /// Renders `[a, b]` with the shared expression list.
    fn bracket_list(&mut self, items: &[Expr]) -> Result<String, FormatError> {
        Ok(format!("[{}]", self.expr_list(items)?))
    }

    /// Renders `{k: v, ...}` with quoted identifiers where needed.
    fn record_fields(&mut self, fields: &[(String, Expr)]) -> Result<String, FormatError> {
        fields
            .iter()
            .map(|(name, value)| Ok(format!("{}: {}", self.ident(name), self.expr(value)?)))
            .collect::<Result<Vec<_>, _>>()
            .map(|rendered| rendered.join(", "))
    }

    /// Renders `AS <type>` for CAST.
    fn value_type_with_as(&self, ty: &ValueType) -> Result<String, FormatError> {
        Ok(format!("{} {}", self.kw("AS"), self.value_type(ty)?))
    }

    /// Renders a value type in the syntax the parser accepts.
    ///
    /// Types that carry their original spelling render it (so `BIGINT` stays `BIGINT`); the rest
    /// render the canonical form. Shapes the parser cannot produce from a rendering (a typed
    /// reference with a label, or string length bounds) stay unsupported rather than being
    /// approximated.
    fn value_type(&self, ty: &ValueType) -> Result<String, FormatError> {
        fn length_suffix(length: Option<u64>) -> String {
            length.map(|n| format!("({n})")).unwrap_or_default()
        }
        Ok(match ty {
            ValueType::Bool { keyword }
            | ValueType::Int8 { keyword }
            | ValueType::Int16 { keyword }
            | ValueType::Int32 { keyword }
            | ValueType::Int64 { keyword }
            | ValueType::Int128 { keyword }
            | ValueType::Int256 { keyword }
            | ValueType::Uint8 { keyword }
            | ValueType::Uint16 { keyword }
            | ValueType::Uint32 { keyword }
            | ValueType::Uint64 { keyword }
            | ValueType::Uint128 { keyword }
            | ValueType::Uint256 { keyword }
            | ValueType::Float16 { keyword }
            | ValueType::Float32 { keyword }
            | ValueType::Float64 { keyword }
            | ValueType::LocalTime { keyword }
            | ValueType::ZonedTime { keyword }
            | ValueType::LocalDateTime { keyword }
            | ValueType::ZonedDateTime { keyword }
            | ValueType::GraphRef { keyword } => self.kw(&keyword.0),
            ValueType::Char { keyword, length } => {
                format!("{}{}", self.kw(&keyword.0), length_suffix(*length))
            }
            ValueType::Varchar {
                keyword,
                max_length,
            } => {
                format!("{}{}", self.kw(&keyword.0), length_suffix(*max_length))
            }
            ValueType::Varbinary {
                keyword,
                max_length,
            } => {
                format!("{}{}", self.kw(&keyword.0), length_suffix(*max_length))
            }
            ValueType::IntPrecision { keyword, precision }
            | ValueType::UintPrecision { keyword, precision } => {
                format!("{}({precision})", self.kw(&keyword.0))
            }
            ValueType::FloatPrecision { precision, scale } => match scale {
                Some(scale) => format!("{}({precision}, {scale})", self.kw("FLOAT")),
                None => format!("{}({precision})", self.kw("FLOAT")),
            },
            ValueType::String {
                min_length,
                max_length,
            } => {
                if min_length.is_some() {
                    return self.unsupported("string length bounds");
                }
                format!("{}{}", self.kw("STRING"), length_suffix(*max_length))
            }
            ValueType::Bytes { max_length } => {
                format!("{}{}", self.kw("BYTES"), length_suffix(*max_length))
            }
            ValueType::Binary { length } => {
                format!("{}{}", self.kw("BINARY"), length_suffix(*length))
            }
            ValueType::Float128 => self.kw("FLOAT128"),
            ValueType::Float256 => self.kw("FLOAT256"),
            ValueType::Decimal {
                keyword,
                precision,
                scale,
            } => match (precision, scale) {
                (Some(precision), Some(scale)) => {
                    format!("{}({precision}, {scale})", self.kw(&keyword.0))
                }
                (Some(precision), None) => format!("{}({precision})", self.kw(&keyword.0)),
                _ => self.kw(&keyword.0),
            },
            ValueType::Date => self.kw("DATE"),
            ValueType::Time => self.kw("TIME"),
            ValueType::DateTime => self.kw("DATETIME"),
            ValueType::Timestamp => self.kw("TIMESTAMP"),
            ValueType::Duration => self.kw("DURATION"),
            ValueType::DurationYearToMonth => {
                format!(
                    "{} {} {}",
                    self.kw("DURATION"),
                    self.kw("YEAR TO"),
                    self.kw("MONTH")
                )
            }
            ValueType::DurationDayToSecond => {
                format!(
                    "{} {} {}",
                    self.kw("DURATION"),
                    self.kw("DAY TO"),
                    self.kw("SECOND")
                )
            }
            ValueType::List {
                keyword,
                element_type,
                max_length,
            } => format!(
                "{}<{}>{}",
                self.kw(&keyword.0),
                self.value_type(element_type)?,
                length_suffix(*max_length)
            ),
            ValueType::Path => self.kw("PATH"),
            ValueType::Record {
                record_keyword,
                fields,
            } => {
                let rendered = self.record_field_types(fields)?;
                if *record_keyword {
                    format!("{} {{{rendered}}}", self.kw("RECORD"))
                } else {
                    format!("{{{rendered}}}")
                }
            }
            ValueType::Any => self.kw("ANY"),
            ValueType::AnyValue => format!("{} {}", self.kw("ANY"), self.kw("VALUE")),
            ValueType::AnyPropertyValue => {
                format!(
                    "{} {} {}",
                    self.kw("ANY"),
                    self.kw("PROPERTY"),
                    self.kw("VALUE")
                )
            }
            ValueType::Nothing => self.kw("NOTHING"),
            ValueType::Null => self.kw("NULL"),
            ValueType::NodeRef { keyword, label } | ValueType::EdgeRef { keyword, label } => {
                if label.is_some() {
                    return self.unsupported("typed reference type");
                }
                self.kw(&keyword.0)
            }
            ValueType::BindingTableRef { fields } => match fields {
                None => format!("{} {}", self.kw("BINDING"), self.kw("TABLE")),
                Some(fields) => format!(
                    "{} {} {{{}}}",
                    self.kw("BINDING"),
                    self.kw("TABLE"),
                    self.record_field_types(fields)?
                ),
            },
            ValueType::ClosedDynamicUnion(members) => members
                .iter()
                .map(|member| self.value_type(member))
                .collect::<Result<Vec<_>, _>>()?
                .join(" | "),
            ValueType::ExtensionType { name } => self.name(name),
            ValueType::NotNull(inner) => {
                format!("{} {}", self.value_type(inner)?, self.kw("NOT NULL"))
            }
        })
    }

    /// Renders record field types as `name TYPE, ...`.
    fn record_field_types(&self, fields: &[RecordFieldType]) -> Result<String, FormatError> {
        fields
            .iter()
            .map(|field| {
                Ok(format!(
                    "{} {}",
                    self.ident(&field.name),
                    self.value_type(&field.value_type)?
                ))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|rendered| rendered.join(", "))
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
            "{}{} {}\n) {} {}",
            self.options.indentation,
            self.kw("LIMIT"),
            self.expr(&spec.limit)?,
            self.kw(metric),
            self.kw("AS")
        ));
        s.push_str(&format!(" {}", alias));
        Ok(s)
    }

    /// Formats a `GRANT`/`REVOKE` statement in canonical form (ADR 0074 §5): the vertex
    /// selector always renders as `NODES` regardless of the parsed keyword.
    #[cfg(feature = "gleaph")]
    fn authorization_change(
        &mut self,
        verb_kw: &str,
        target: &GrantTarget,
        subject: &GrantSubjectLiteral,
        subject_preposition_kw: &str,
    ) -> Result<String, FormatError> {
        let subject_text = match subject {
            GrantSubjectLiteral::Principal(text) => {
                format!("{} '{}'", self.kw("PRINCIPAL"), text)
            }
            GrantSubjectLiteral::Public => self.kw("PUBLIC"),
        };
        match target {
            GrantTarget::Graph {
                privilege,
                graph,
                resource,
                condition,
            } => {
                let privilege_text = match privilege {
                    GrantPrivilege::Match => self.kw("MATCH"),
                    GrantPrivilege::Traverse { direction } => match direction {
                        Some(GrantDirection::Outgoing) => {
                            format!("{} {}", self.kw("TRAVERSE"), self.kw("OUTGOING"))
                        }
                        Some(GrantDirection::Incoming) => {
                            format!("{} {}", self.kw("TRAVERSE"), self.kw("INCOMING"))
                        }
                        None => self.kw("TRAVERSE"),
                    },
                    GrantPrivilege::Read { properties } => {
                        if properties.is_empty() {
                            self.kw("READ")
                        } else {
                            let list = properties
                                .iter()
                                .map(|p| p.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{} {{{}}}", self.kw("READ"), list)
                        }
                    }
                    GrantPrivilege::Create => self.kw("CREATE"),
                    GrantPrivilege::Update => self.kw("UPDATE"),
                    GrantPrivilege::Delete => self.kw("DELETE"),
                };
                let resource_text = match resource {
                    GrantResourceSelector::Vertex { label } => {
                        format!("{} {}", self.kw("NODES"), label)
                    }
                    GrantResourceSelector::Edge { label } => {
                        format!("{} {}", self.kw("EDGES"), label)
                    }
                    // `NODES *` (ADR 0089 §5): the wildcard selector has no label to render.
                    GrantResourceSelector::AllVertexLabels => format!("{} *", self.kw("NODES")),
                };
                let condition_text = match condition {
                    None => String::new(),
                    Some(condition) => {
                        let predicate = self.authorization_condition(
                            &condition.predicate,
                            &condition.selector,
                            condition.chain.as_deref(),
                        )?;
                        format!(
                            " {} {}",
                            self.authorization_condition_selector(&condition.selector),
                            predicate
                        )
                    }
                };
                Ok(format!(
                    "{} {} {} {} {}{} {} {}",
                    self.kw(verb_kw),
                    privilege_text,
                    self.kw("ON GRAPH"),
                    self.name(graph),
                    resource_text,
                    condition_text,
                    self.kw(subject_preposition_kw),
                    subject_text
                ))
            }
            GrantTarget::PreparedQuery { name } => Ok(format!(
                "{} {} {} {} {} {}",
                self.kw(verb_kw),
                self.kw("EXECUTE"),
                self.kw("ON PREPARED QUERY"),
                name,
                self.kw(subject_preposition_kw),
                subject_text
            )),
            GrantTarget::Metadata { scope } => {
                let scope_text = match scope {
                    GrantMetadataScope::Graph(graph) => {
                        format!("{} {}", self.kw("READ_METADATA ON GRAPH"), self.name(graph))
                    }
                    GrantMetadataScope::ControlPlane => self.kw("READ_METADATA ON CONTROL PLANE"),
                };
                Ok(format!(
                    "{} {} {} {}",
                    self.kw(verb_kw),
                    scope_text,
                    self.kw(subject_preposition_kw),
                    subject_text
                ))
            }
        }
    }

    /// Renders a conditional selector in canonical form ([ADR 0075] §3): the vertex form
    /// as `FOR (v:Label)`, the edge form undirected as `FOR ()-[e:Label]-()`.
    #[cfg(feature = "gleaph")]
    fn authorization_condition_selector(&self, selector: &GrantConditionSelector) -> String {
        match selector {
            GrantConditionSelector::Vertex { variable, label } => {
                format!("{} ({}:{})", self.kw("FOR"), variable, label)
            }
            GrantConditionSelector::Edge { variable, label } => {
                format!("{} ()-[{}:{}]-()", self.kw("FOR"), variable, label)
            }
        }
    }

    /// Renders the AND-ordered predicate body (`WHERE v.p = 'x' AND v.q = MSG_CALLER()`)
    /// plus the optional bounded EXISTS chain inline ([ADR 0082] §2).
    #[cfg(feature = "gleaph")]
    fn authorization_condition(
        &mut self,
        predicate: &GrantPredicate,
        selector: &GrantConditionSelector,
        chain: Option<&GrantChain>,
    ) -> Result<String, FormatError> {
        let variable = selector.variable().to_owned();
        let mut out = self.kw("WHERE");
        for (index, conjunct) in predicate.conjuncts.iter().enumerate() {
            if index > 0 {
                out.push_str(&format!(" {}", self.kw("AND")));
            }
            out.push_str(&format!(" {}", self.comparison_text(conjunct, &variable)?));
        }
        if let Some(chain) = chain {
            if !predicate.conjuncts.is_empty() {
                out.push_str(&format!(" {}", self.kw("AND")));
            }
            out.push_str(&format!(" {} {{ ", self.kw("EXISTS")));
            out.push_str(&format!("({})", chain.source_variable));
            for hop in &chain.hops {
                match hop.direction {
                    GrantChainDirection::Right => out.push_str(&format!(
                        "-[:{}]->({}:{})",
                        hop.edge_label, hop.variable, hop.label
                    )),
                    GrantChainDirection::Left => out.push_str(&format!(
                        "<-[:{}]-({}:{})",
                        hop.edge_label, hop.variable, hop.label
                    )),
                    GrantChainDirection::Undirected => out.push_str(&format!(
                        "-[:{}]-({}:{})",
                        hop.edge_label, hop.variable, hop.label
                    )),
                }
            }
            out.push_str(&format!(" {} ", self.kw("WHERE")));
            let terminal_variable = chain
                .hops
                .last()
                .map(|hop| hop.variable.as_str())
                .unwrap_or(chain.source_variable.as_str());
            for (index, conjunct) in chain.terminal_conjuncts.iter().enumerate() {
                if index > 0 {
                    out.push_str(&format!(" {}", self.kw("AND")));
                }
                out.push_str(&format!(
                    " {}",
                    self.comparison_text(conjunct, terminal_variable)?
                ));
            }
            out.push_str(" }");
        }
        Ok(out)
    }

    /// Renders one `<variable>.<property> <op> <value>` comparison.
    #[cfg(feature = "gleaph")]
    fn comparison_text(
        &mut self,
        conjunct: &GrantComparison,
        variable: &str,
    ) -> Result<String, FormatError> {
        let value = match &conjunct.value {
            GrantValueExpr::Literal(value) => {
                self.expr(&Expr::new(ExprKind::Literal(value.clone())))?
            }
            GrantValueExpr::MsgCaller => "MSG_CALLER()".to_owned(),
        };
        let op = match conjunct.op {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        };
        Ok(format!(
            "{}.{} {} {}",
            variable, conjunct.property, op, value
        ))
    }
}

impl fmt::Display for KeywordCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[cfg(all(test, feature = "gleaph"))]
mod metadata_grant_formatting_tests {
    use crate::format::{FormatOptions, format_query};

    /// Metadata elevation statements round-trip through the canonical formatter
    /// ([ADR 0080] §5): the formatted form reparses and formats to itself (fixed point).
    #[test]
    fn read_metadata_forms_round_trip_through_the_canonical_formatter() {
        for source in [
            "GRANT READ_METADATA ON GRAPH social TO PRINCIPAL 'w7x7r-cok77-xa'",
            "GRANT READ_METADATA ON CONTROL PLANE TO PUBLIC",
            "REVOKE READ_METADATA ON GRAPH social FROM PRINCIPAL 'w7x7r-cok77-xa'",
            "REVOKE READ_METADATA ON CONTROL PLANE FROM PUBLIC",
        ] {
            let formatted = format_query(source, &FormatOptions::default())
                .unwrap_or_else(|e| panic!("format {source}: {e}"));
            assert_eq!(
                format_query(&formatted, &FormatOptions::default())
                    .expect("formatted form must stay parseable"),
                formatted,
                "canonical form of {source} must be a formatting fixed point"
            );
        }
    }
}

/// True when `s` can be spelled in GQL source without delimited-identifier quoting.
fn is_plain_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_alphabetic() || first == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
}
