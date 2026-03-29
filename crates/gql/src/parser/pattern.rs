//! Graph pattern parser (§16 of GQL).
//!
//! Implements parsing for graph patterns, path patterns, node/edge patterns,
//! label expressions, quantifiers, insert patterns, and simplified path
//! patterns.

use crate::ast::{
    EdgePattern, GraphPattern, GroupOrGroups, InsertEdgePattern, InsertElement, InsertNodePattern,
    InsertPathPattern, IsOrColon, KeepClause, MatchMode, MatchModeEdgeKeyword,
    MatchModeElementKeyword, NodePattern, PathFactor, PathMode, PathOrPaths, PathPattern,
    PathPatternExpr, PathPatternPrefix, PathPrimary, PathQuantifier, PathTerm, PropertySetting,
    SearchPrefix, SimplifiedContents, SimplifiedElement, SimplifiedPathPattern,
};
use crate::error::GqlError;
use crate::token::Token;
use crate::types::{EdgeDirection, LabelExpr};

use super::helpers::Parser;

impl Parser<'_> {
    // ════════════════════════════════════════════════════════════════════════
    // §16.4 — Graph pattern
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a graph pattern:
    ///   `[matchMode] pathPatternList [keepClause] [WHERE searchCondition]`
    pub fn parse_graph_pattern(&mut self) -> Result<GraphPattern, GqlError> {
        let start = self.save();
        let match_mode = self.parse_match_mode()?;

        let paths = self.comma_list(|p| p.parse_path_pattern())?;

        let keep = self.parse_keep_clause()?;

        let where_clause = if self.eat_keyword("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(GraphPattern {
            span: self.span_since(start),
            match_mode,
            paths,
            keep,
            where_clause,
        })
    }

    /// Parses an optional match mode.
    fn parse_match_mode(&mut self) -> Result<Option<MatchMode>, GqlError> {
        if self.eat_keyword("REPEATABLE") {
            // REPEATABLE ELEMENT [BINDINGS] | REPEATABLE ELEMENTS
            let keyword = if self.eat_keyword("ELEMENT") {
                if self.eat_keyword("BINDINGS") {
                    MatchModeElementKeyword::ElementBindings
                } else {
                    MatchModeElementKeyword::Element
                }
            } else if self.eat_keyword("ELEMENTS") {
                MatchModeElementKeyword::Elements
            } else {
                return Err(self.expected("'ELEMENT' or 'ELEMENTS'"));
            };
            Ok(Some(MatchMode::RepeatableElements { keyword }))
        } else if self.eat_keyword("DIFFERENT") {
            // DIFFERENT EDGE [BINDINGS] | DIFFERENT EDGES
            // DIFFERENT RELATIONSHIP [BINDINGS] | DIFFERENT RELATIONSHIPS
            let keyword = if self.eat_keyword("EDGE") {
                if self.eat_keyword("BINDINGS") {
                    MatchModeEdgeKeyword::EdgeBindings
                } else {
                    MatchModeEdgeKeyword::Edge
                }
            } else if self.eat_keyword("RELATIONSHIP") {
                if self.eat_keyword("BINDINGS") {
                    MatchModeEdgeKeyword::RelationshipBindings
                } else {
                    MatchModeEdgeKeyword::Relationship
                }
            } else if self.eat_keyword("EDGES") {
                MatchModeEdgeKeyword::Edges
            } else if self.eat_keyword("RELATIONSHIPS") {
                MatchModeEdgeKeyword::Relationships
            } else {
                return Err(self.expected("'EDGE', 'EDGES', 'RELATIONSHIP', or 'RELATIONSHIPS'"));
            };
            Ok(Some(MatchMode::DifferentEdges { keyword }))
        } else {
            Ok(None)
        }
    }

    /// Parses an optional KEEP clause (GQL: `keepClause: KEEP pathPatternPrefix`).
    fn parse_keep_clause(&mut self) -> Result<Option<KeepClause>, GqlError> {
        let start = self.save();
        if !self.eat_keyword("KEEP") {
            return Ok(None);
        }
        // KEEP takes a path pattern prefix (mode or search prefix).
        if let Some(prefix) = self.parse_path_pattern_prefix()? {
            Ok(Some(KeepClause {
                span: self.span_since(start),
                prefix,
            }))
        } else {
            Err(self.expected("path pattern prefix after KEEP (WALK, TRAIL, SIMPLE, ACYCLIC, ALL, ANY, SHORTEST, etc.)"))
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.3/16.7 — Path pattern
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a path pattern:
    ///   `[pathVariable '='] [pathPatternPrefix] pathPatternExpression`
    pub fn parse_path_pattern(&mut self) -> Result<PathPattern, GqlError> {
        let start = self.save();
        // Optional path variable declaration: `var =`
        let variable = self.try_parse_path_variable_decl();

        // Optional prefix (path mode or search prefix).
        let prefix = self.parse_path_pattern_prefix()?;

        // The path pattern expression itself.
        let expr = self.parse_path_pattern_expr()?;

        Ok(PathPattern {
            span: self.span_since(start),
            variable,
            prefix,
            expr,
        })
    }

    /// Tries to parse `identifier =` as a path variable declaration.
    /// Uses backtracking: if the next two tokens are not `ident =`, restores
    /// position and returns None.
    fn try_parse_path_variable_decl(&mut self) -> Option<String> {
        let save = self.save();
        if let Some(name) = self.eat_ident_non_reserved() {
            if self.eat_token(&Token::Eq) {
                return Some(name);
            }
            self.restore(save);
        }
        None
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.6 — Path pattern prefix
    // ════════════════════════════════════════════════════════════════════════

    /// Parses an optional path pattern prefix (mode or search).
    fn parse_path_pattern_prefix(&mut self) -> Result<Option<PathPatternPrefix>, GqlError> {
        // Try search prefixes first (ALL, ANY, SHORTEST, COUNT).
        if let Some(search) = self.try_parse_search_prefix()? {
            return Ok(Some(PathPatternPrefix::Search(search)));
        }
        // Try path mode prefix (WALK, TRAIL, SIMPLE, ACYCLIC).
        if let Some(mode) = self.try_parse_path_mode() {
            let path_keyword = self.eat_path_or_paths();
            return Ok(Some(PathPatternPrefix::Mode { mode, path_keyword }));
        }
        Ok(None)
    }

    /// Tries to parse a path mode keyword.
    fn try_parse_path_mode(&mut self) -> Option<PathMode> {
        if self.eat_keyword("WALK") {
            Some(PathMode::Walk)
        } else if self.eat_keyword("TRAIL") {
            Some(PathMode::Trail)
        } else if self.eat_keyword("SIMPLE") {
            Some(PathMode::Simple)
        } else if self.eat_keyword("ACYCLIC") {
            Some(PathMode::Acyclic)
        } else {
            None
        }
    }

    /// Consumes an optional PATH or PATHS keyword and returns which was used.
    fn eat_path_or_paths(&mut self) -> Option<PathOrPaths> {
        if self.eat_keyword("PATH") {
            Some(PathOrPaths::Path)
        } else if self.eat_keyword("PATHS") {
            Some(PathOrPaths::Paths)
        } else {
            None
        }
    }

    /// Tries to parse a search prefix (ALL, ANY, SHORTEST, COUNT).
    fn try_parse_search_prefix(&mut self) -> Result<Option<SearchPrefix>, GqlError> {
        if self.at_keyword("ALL") {
            let _save = self.save();
            self.advance(); // consume ALL

            if self.eat_keyword("SHORTEST") {
                let mode = self.try_parse_path_mode();
                let path_keyword = self.eat_path_or_paths();
                return Ok(Some(SearchPrefix::AllShortest { mode, path_keyword }));
            }

            // ALL [pathMode] [PATH|PATHS]
            let mode = self.try_parse_path_mode();
            // Check if this is really a search prefix: we need to see
            // PATH/PATHS or a path pattern start after it.
            let path_keyword = self.eat_path_or_paths();

            // If we consumed only ALL and the next token is not a pattern
            // start, we might have an ALL that's part of something else.
            // However, ALL as a search prefix is valid even alone.
            return Ok(Some(SearchPrefix::All { mode, path_keyword }));
        }

        if self.at_keyword("ANY") {
            self.advance(); // consume ANY

            if self.eat_keyword("SHORTEST") {
                let mode = self.try_parse_path_mode();
                let path_keyword = self.eat_path_or_paths();
                return Ok(Some(SearchPrefix::AnyShortest { mode, path_keyword }));
            }

            // ANY [numberOfPaths] [pathMode] [PATH|PATHS]
            let k = self.try_parse_number_of_paths();
            let mode = self.try_parse_path_mode();
            let path_keyword = self.eat_path_or_paths();
            return Ok(Some(SearchPrefix::Any {
                k,
                mode,
                path_keyword,
            }));
        }

        if self.at_keyword("SHORTEST") {
            self.advance(); // consume SHORTEST

            // SHORTEST <k> [pathMode] [PATH|PATHS] [GROUP[S]]
            // or SHORTEST [k] [pathMode] [PATH|PATHS] GROUP[S]
            let k = self.try_parse_number_of_paths();
            let mode = self.try_parse_path_mode();
            let path_keyword = self.eat_path_or_paths();

            if self.at_keyword("GROUP") || self.at_keyword("GROUPS") {
                let group_keyword = if self.eat_keyword("GROUP") {
                    GroupOrGroups::Group
                } else {
                    self.advance(); // GROUPS
                    GroupOrGroups::Groups
                };
                return Ok(Some(SearchPrefix::ShortestKGroup {
                    k: k.unwrap_or(1),
                    mode,
                    path_keyword,
                    group_keyword,
                }));
            }

            return Ok(Some(SearchPrefix::ShortestK {
                k: k.unwrap_or(1),
                mode,
                path_keyword,
            }));
        }

        // COUNT PATHS — cypher extension (not in GQL).
        #[cfg(feature = "cypher")]
        if self.at_keyword("COUNT") {
            self.advance(); // consume COUNT
            self.expect_keyword("PATHS")?;
            let mode = self.try_parse_path_mode();
            return Ok(Some(SearchPrefix::CountPaths {
                mode,
                path_keyword: Some(PathOrPaths::Paths),
            }));
        }

        Ok(None)
    }

    /// Tries to parse an unsigned integer (numberOfPaths / numberOfGroups).
    fn try_parse_number_of_paths(&mut self) -> Option<u64> {
        match self.peek() {
            Some(Token::Int(v)) if *v >= 0 => {
                let v = *v as u64;
                self.advance();
                Some(v)
            }
            _ => None,
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.7 — Path pattern expression
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a path pattern expression:
    ///   pathTerm ( '|' pathTerm )* — pattern union
    ///   pathTerm ( '|+|' pathTerm )* — multiset alternation
    ///   pathTerm — single term
    fn parse_path_pattern_expr(&mut self) -> Result<PathPatternExpr, GqlError> {
        let first = self.parse_path_term()?;

        if self.at_token(&Token::MultisetAlt) {
            let mut terms = vec![first];
            while self.eat_token(&Token::MultisetAlt) {
                terms.push(self.parse_path_term()?);
            }
            return Ok(PathPatternExpr::MultisetAlternation(terms));
        }

        if self.at_token(&Token::Pipe) {
            let mut terms = vec![first];
            while self.eat_token(&Token::Pipe) {
                terms.push(self.parse_path_term()?);
            }
            return Ok(PathPatternExpr::PatternUnion(terms));
        }

        Ok(PathPatternExpr::Term(first))
    }

    /// Parses a path term: one or more path factors concatenated.
    fn parse_path_term(&mut self) -> Result<PathTerm, GqlError> {
        let start = self.save();
        let mut factors = vec![self.parse_path_factor()?];
        while self.at_path_primary_start() {
            factors.push(self.parse_path_factor()?);
        }
        Ok(PathTerm {
            span: self.span_since(start),
            factors,
        })
    }

    /// Returns true if the current token can start a path primary.
    fn at_path_primary_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                // Node pattern or parenthesized path pattern
                Token::LParen
                // Full edge patterns
                | Token::MinusLeftBracket
                | Token::LeftArrowBracket
                | Token::TildeLeftBracket
                | Token::LeftArrowTildeBracket
                // Abbreviated edges
                | Token::RightArrow
                | Token::LeftArrow
                | Token::LeftMinusRight
                | Token::TildeRightArrow
                | Token::LeftArrowTilde
                | Token::Tilde
                | Token::Minus
                // Simplified path patterns
                | Token::MinusSlash
                | Token::LeftMinusSlash
                | Token::TildeSlash
                | Token::LeftTildeSlash
            )
        )
    }

    /// Parses a path factor: pathPrimary [quantifier | '?']
    fn parse_path_factor(&mut self) -> Result<PathFactor, GqlError> {
        let start = self.save();
        let primary = self.parse_path_primary()?;
        let quantifier = self.try_parse_quantifier()?;
        Ok(PathFactor {
            span: self.span_since(start),
            primary,
            quantifier,
        })
    }

    /// Parses a path primary:
    ///   - node pattern `(…)`
    ///   - edge pattern
    ///   - parenthesized path pattern expression `(…)`
    ///   - simplified path pattern expression
    fn parse_path_primary(&mut self) -> Result<PathPrimary, GqlError> {
        // Simplified path patterns: direction tokens with `/`.
        if self.at_simplified_start() {
            let sp = self.parse_simplified_path_pattern_expr()?;
            return Ok(PathPrimary::Simplified(sp));
        }

        // Edge patterns — check full edges first, then abbreviated.
        if self.at_full_edge_start() {
            let edge = self.parse_edge_pattern()?;
            return Ok(PathPrimary::Edge(edge));
        }
        if self.at_abbreviated_edge() {
            let edge = self.parse_abbreviated_edge()?;
            return Ok(PathPrimary::Edge(edge));
        }

        // LParen: either node pattern or parenthesized path pattern.
        if self.at_token(&Token::LParen) {
            return self.parse_node_or_parenthesized();
        }

        Err(self.expected("node pattern, edge pattern, or path expression"))
    }

    /// Distinguishes between a node pattern `(filler)` and a parenthesized
    /// path pattern expression `([var =] [mode] pathExpr [WHERE …])`.
    ///
    /// Heuristic: if after `(` we see a sub-path variable `ident =` followed
    /// by another path-like token, or if we see a path mode keyword, treat it
    /// as parenthesized. Otherwise, treat as a node pattern.
    fn parse_node_or_parenthesized(&mut self) -> Result<PathPrimary, GqlError> {
        // We need to decide: is `(…)` a node pattern or a parenthesized path?
        // A parenthesized path can contain: [subpathVar =] [pathMode] pathExpr [WHERE …]
        // A node filler contains: [var] [:label|IS label] [{props}|WHERE …]
        //
        // Key differentiator: if we see a path mode keyword after `(`, or if
        // the content after an optional `var =` starts with a path element
        // (another `(`, edge token), it's parenthesized.
        let save = self.save();

        // Speculatively try parenthesized path pattern.
        if let Ok(result) = self.try_parse_parenthesized_path() {
            return Ok(result);
        }
        self.restore(save);

        // Fall back to node pattern.
        let node = self.parse_node_pattern()?;
        Ok(PathPrimary::Node(node))
    }

    /// Tries to parse a parenthesized path pattern expression.
    fn try_parse_parenthesized_path(&mut self) -> Result<PathPrimary, GqlError> {
        self.expect_token(&Token::LParen)?;

        // Optional subpath variable: `var =`
        let subpath_var = self.try_parse_path_variable_decl();

        // Optional path mode.
        let mode = self.try_parse_path_mode();
        let path_keyword = if mode.is_some() {
            self.eat_path_or_paths()
        } else {
            None
        };

        // If we consumed a mode, this must be parenthesized.
        // If we consumed a subpath var, check what follows.
        // Otherwise, we need at least two path factors or a recognizable pattern.
        if mode.is_none() && subpath_var.is_none() {
            // Must see something that could only be a path pattern expression,
            // not a node filler. If we see an edge or nested `(`, it's a path.
            if !self.at_path_primary_start() {
                return Err(self.error("not a parenthesized path pattern"));
            }
        }

        let expr = self.parse_path_pattern_expr()?;

        let where_clause = if self.eat_keyword("WHERE") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };

        self.expect_token(&Token::RParen)?;

        Ok(PathPrimary::Parenthesized {
            variable: subpath_var,
            mode,
            path_keyword,
            expr: Box::new(expr),
            where_clause,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.8 — Node pattern
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a node pattern: `( [var] [':' labelExpr | IS labelExpr] [{props}|WHERE cond] )`
    pub fn parse_node_pattern(&mut self) -> Result<NodePattern, GqlError> {
        let start = self.save();
        self.expect_token(&Token::LParen)?;
        let (variable, is_or_colon, label, properties, where_clause) =
            self.parse_element_pattern_filler()?;
        self.expect_token(&Token::RParen)?;

        Ok(NodePattern {
            span: self.span_since(start),
            variable,
            is_or_colon,
            label,
            properties,
            where_clause,
        })
    }

    /// Parses the content inside a node or edge pattern:
    ///   `[var] [':' labelExpr | IS labelExpr] [{props} | WHERE cond]`
    #[allow(clippy::type_complexity)]
    fn parse_element_pattern_filler(
        &mut self,
    ) -> Result<
        (
            Option<String>,
            Option<IsOrColon>,
            Option<LabelExpr>,
            Vec<PropertySetting>,
            Option<Box<Expr>>,
        ),
        GqlError,
    > {
        // Optional variable name.
        // The variable is an identifier that is NOT a reserved keyword.
        // We must be careful not to consume IS, WHERE, etc.
        let variable = self.try_parse_element_variable();

        // Optional label expression: `:labelExpr` or `IS labelExpr`.
        let (is_or_colon, label) = if self.eat_token(&Token::Colon) {
            (Some(IsOrColon::Colon), Some(self.parse_label_expr()?))
        } else if self.eat_keyword("IS") {
            (Some(IsOrColon::Is), Some(self.parse_label_expr()?))
        } else {
            (None, None)
        };

        // Optional predicate: WHERE or {properties}.
        let mut properties = vec![];
        let mut where_clause = None;

        if self.eat_keyword("WHERE") {
            where_clause = Some(Box::new(self.parse_expr()?));
        } else if self.at_token(&Token::LBrace) {
            properties = self.parse_property_map()?;
        }

        Ok((variable, is_or_colon, label, properties, where_clause))
    }

    /// Tries to parse an element variable (non-reserved identifier).
    /// Does NOT consume IS, WHERE, or colon-preceded labels.
    fn try_parse_element_variable(&mut self) -> Option<String> {
        // Don't consume if next is `:`, `IS`, `WHERE`, `{`, `)`, `]`, or a
        // closing bracket token — those indicate no variable is present.
        match self.peek() {
            Some(Token::Colon)
            | Some(Token::LBrace)
            | Some(Token::RParen)
            | Some(Token::RBracket)
            | Some(Token::RightBracketMinus)
            | Some(Token::BracketRightArrow)
            | Some(Token::RightBracketTilde)
            | Some(Token::BracketTildeRightArrow) => return None,
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("IS") => return None,
            Some(Token::Ident(s)) if s.eq_ignore_ascii_case("WHERE") => return None,
            _ => {}
        }
        self.eat_ident_non_reserved()
    }

    /// Parses a property map: `{ key: value, key: value, ... }`
    fn parse_property_map(&mut self) -> Result<Vec<PropertySetting>, GqlError> {
        self.expect_token(&Token::LBrace)?;
        let mut props = vec![];
        if !self.at_token(&Token::RBrace) {
            loop {
                let prop_start = self.save();
                let name = self.expect_ident()?;
                self.expect_token(&Token::Colon)?;
                let value = self.parse_expr()?;
                props.push(PropertySetting {
                    span: self.span_since(prop_start),
                    name,
                    value,
                });
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
        }
        self.expect_token(&Token::RBrace)?;
        Ok(props)
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.9 — Edge pattern
    // ════════════════════════════════════════════════════════════════════════

    /// Returns true if the current token starts a full edge pattern.
    fn at_full_edge_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Token::MinusLeftBracket
                    | Token::LeftArrowBracket
                    | Token::TildeLeftBracket
                    | Token::LeftArrowTildeBracket
            )
        )
    }

    /// Returns true if the current token is an abbreviated edge.
    fn at_abbreviated_edge(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Token::RightArrow
                    | Token::LeftArrow
                    | Token::LeftMinusRight
                    | Token::TildeRightArrow
                    | Token::LeftArrowTilde
                    | Token::Tilde
                    | Token::Minus
            )
        )
    }

    /// Parses a full edge pattern with bracket notation.
    pub fn parse_edge_pattern(&mut self) -> Result<EdgePattern, GqlError> {
        let start = self.save();
        let (direction, closing_tokens) = self.parse_edge_opening()?;
        let (variable, is_or_colon, label, properties, where_clause) =
            self.parse_element_pattern_filler()?;
        self.expect_edge_closing(&closing_tokens)?;

        Ok(EdgePattern {
            span: self.span_since(start),
            direction,
            variable,
            is_or_colon,
            label,
            properties,
            where_clause,
        })
    }

    /// Parses the opening token of a full edge and returns (direction, expected
    /// closing token(s)).
    fn parse_edge_opening(&mut self) -> Result<(EdgeDirection, Token), GqlError> {
        match self.peek() {
            Some(Token::MinusLeftBracket) => {
                self.advance();
                // Could be `-[…]->` (right) or `-[…]-` (any direction).
                // We decide at closing time.
                // Peek ahead after filler to determine direction.
                // For now, return a placeholder; we'll resolve at closing.
                Ok((EdgeDirection::PointingRight, Token::BracketRightArrow))
            }
            Some(Token::LeftArrowBracket) => {
                self.advance();
                // Could be `<-[…]-` (left) or `<-[…]->` (left or right).
                Ok((EdgeDirection::PointingLeft, Token::RightBracketMinus))
            }
            Some(Token::TildeLeftBracket) => {
                self.advance();
                // Could be `~[…]~` (undirected) or `~[…]~>` (undirected or right).
                Ok((EdgeDirection::Undirected, Token::RightBracketTilde))
            }
            Some(Token::LeftArrowTildeBracket) => {
                self.advance();
                // `<~[…]~` (left or undirected)
                Ok((EdgeDirection::LeftOrUndirected, Token::RightBracketTilde))
            }
            _ => Err(self.expected("edge opening token")),
        }
    }

    /// Consumes the closing token of a full edge pattern and may adjust
    /// direction based on what closing was actually found.
    fn expect_edge_closing(&mut self, expected: &Token) -> Result<EdgeDirection, GqlError> {
        // The opening gives a "default" expected close. However, some openings
        // are ambiguous:
        //   `-[` can close with `]->` (right) or `]-` (any direction)
        //   `<-[` can close with `]-` (left) or `]->` (left or right)
        //   `~[` can close with `]~` (undirected) or `]~>` (undirected or right)

        match self.peek() {
            Some(Token::BracketRightArrow) => {
                self.advance();
                Ok(EdgeDirection::PointingRight)
            }
            Some(Token::RightBracketMinus) => {
                self.advance();
                Ok(EdgeDirection::PointingLeft) // or LeftOrUndirectedOrRight, resolved below
            }
            Some(Token::RightBracketTilde) => {
                self.advance();
                Ok(EdgeDirection::Undirected)
            }
            Some(Token::BracketTildeRightArrow) => {
                self.advance();
                Ok(EdgeDirection::UndirectedOrRight)
            }
            _ => {
                self.expect_token(expected)?;
                Ok(EdgeDirection::AnyDirection)
            }
        }
    }

    /// Re-parses a full edge pattern, correctly resolving direction from
    /// the combination of opening and closing tokens.
    pub fn parse_full_edge_pattern(&mut self) -> Result<EdgePattern, GqlError> {
        let start = self.save();
        let opening = self.advance().clone();
        let (variable, is_or_colon, label, properties, where_clause) =
            self.parse_element_pattern_filler()?;

        let closing = self.advance().clone();
        let direction = Self::resolve_edge_direction(&opening, &closing)?;

        Ok(EdgePattern {
            span: self.span_since(start),
            direction,
            variable,
            is_or_colon,
            label,
            properties,
            where_clause,
        })
    }

    /// Resolves the edge direction from the combination of opening and closing
    /// bracket tokens.
    fn resolve_edge_direction(opening: &Token, closing: &Token) -> Result<EdgeDirection, GqlError> {
        match (opening, closing) {
            // -[…]-> : right
            (Token::MinusLeftBracket, Token::BracketRightArrow) => Ok(EdgeDirection::PointingRight),
            // -[…]- : any direction
            (Token::MinusLeftBracket, Token::RightBracketMinus) => Ok(EdgeDirection::AnyDirection),
            // <-[…]- : left
            (Token::LeftArrowBracket, Token::RightBracketMinus) => Ok(EdgeDirection::PointingLeft),
            // <-[…]-> : left or right
            (Token::LeftArrowBracket, Token::BracketRightArrow) => Ok(EdgeDirection::LeftOrRight),
            // ~[…]~ : undirected
            (Token::TildeLeftBracket, Token::RightBracketTilde) => Ok(EdgeDirection::Undirected),
            // ~[…]~> : undirected or right
            (Token::TildeLeftBracket, Token::BracketTildeRightArrow) => {
                Ok(EdgeDirection::UndirectedOrRight)
            }
            // <~[…]~ : left or undirected
            (Token::LeftArrowTildeBracket, Token::RightBracketTilde) => {
                Ok(EdgeDirection::LeftOrUndirected)
            }
            _ => Err(GqlError::Parse(format!(
                "invalid edge bracket combination: {:?} … {:?}",
                opening, closing
            ))),
        }
    }

    /// Parses an abbreviated (single-token) edge pattern.
    fn parse_abbreviated_edge(&mut self) -> Result<EdgePattern, GqlError> {
        let start = self.save();
        let direction = match self.peek() {
            Some(Token::RightArrow) => EdgeDirection::PointingRight,
            Some(Token::LeftArrow) => EdgeDirection::PointingLeft,
            Some(Token::Tilde) => EdgeDirection::Undirected,
            Some(Token::LeftArrowTilde) => EdgeDirection::LeftOrUndirected,
            Some(Token::TildeRightArrow) => EdgeDirection::UndirectedOrRight,
            Some(Token::LeftMinusRight) => EdgeDirection::LeftOrRight,
            Some(Token::Minus) => EdgeDirection::AnyDirection,
            _ => return Err(self.expected("abbreviated edge pattern")),
        };
        self.advance();

        Ok(EdgePattern {
            span: self.span_since(start),
            direction,
            variable: None,
            is_or_colon: None,
            label: None,
            properties: vec![],
            where_clause: None,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.8 — Label expression
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a label expression with precedence:
    ///   NOT (!) > AND (&) > OR (|)
    pub fn parse_label_expr(&mut self) -> Result<LabelExpr, GqlError> {
        self.parse_label_or()
    }

    /// Parses label OR: `a | b`
    fn parse_label_or(&mut self) -> Result<LabelExpr, GqlError> {
        let mut left = self.parse_label_and()?;
        while self.eat_token(&Token::Pipe) {
            let right = self.parse_label_and()?;
            left = LabelExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Parses label AND: `a & b`
    fn parse_label_and(&mut self) -> Result<LabelExpr, GqlError> {
        let mut left = self.parse_label_not()?;
        while self.eat_token(&Token::Ampersand) {
            let right = self.parse_label_not()?;
            left = LabelExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// Parses label NOT: `!a`
    fn parse_label_not(&mut self) -> Result<LabelExpr, GqlError> {
        if self.eat_token(&Token::Bang) {
            let inner = self.parse_label_primary()?;
            Ok(LabelExpr::Not(Box::new(inner)))
        } else {
            self.parse_label_primary()
        }
    }

    /// Parses a label primary: name, `%` wildcard, or `(expr)`.
    fn parse_label_primary(&mut self) -> Result<LabelExpr, GqlError> {
        if self.eat_token(&Token::Percent) {
            return Ok(LabelExpr::Wildcard);
        }
        if self.eat_token(&Token::LParen) {
            let expr = self.parse_label_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(expr);
        }
        // Label name — an identifier.
        let name = self.expect_ident()?;
        Ok(LabelExpr::Name(name))
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.11 — Path quantifier
    // ════════════════════════════════════════════════════════════════════════

    /// Tries to parse an optional path quantifier: `*`, `+`, `?`, `{n}`, or `{n,m}`.
    fn try_parse_quantifier(&mut self) -> Result<Option<PathQuantifier>, GqlError> {
        if self.eat_token(&Token::Star) {
            return Ok(Some(PathQuantifier::Star));
        }
        if self.eat_token(&Token::Plus) {
            return Ok(Some(PathQuantifier::Plus));
        }
        if self.eat_token(&Token::Question) {
            return Ok(Some(PathQuantifier::Optional));
        }
        if self.at_token(&Token::LBrace) {
            return Ok(Some(self.parse_brace_quantifier()?));
        }
        Ok(None)
    }

    /// Parses a quantifier: `{n}`, `{n,}`, `{n,m}`, `{,m}`.
    pub fn parse_quantifier(&mut self) -> Result<PathQuantifier, GqlError> {
        if self.eat_token(&Token::Star) {
            return Ok(PathQuantifier::Star);
        }
        if self.eat_token(&Token::Plus) {
            return Ok(PathQuantifier::Plus);
        }
        if self.eat_token(&Token::Question) {
            return Ok(PathQuantifier::Optional);
        }
        self.parse_brace_quantifier()
    }

    /// Parses `{n}` or `{n,m}` or `{n,}` or `{,m}`.
    fn parse_brace_quantifier(&mut self) -> Result<PathQuantifier, GqlError> {
        self.expect_token(&Token::LBrace)?;

        // {,m} — no lower bound
        if self.at_token(&Token::Comma) {
            self.advance();
            let upper = self.expect_unsigned_int()?;
            self.expect_token(&Token::RBrace)?;
            return Ok(PathQuantifier::Range {
                lower: 0,
                upper: Some(upper),
            });
        }

        let n = self.expect_unsigned_int()?;

        if self.eat_token(&Token::Comma) {
            // {n,m} or {n,}
            let upper = if self.at_token(&Token::RBrace) {
                None
            } else {
                Some(self.expect_unsigned_int()?)
            };
            self.expect_token(&Token::RBrace)?;
            Ok(PathQuantifier::Range { lower: n, upper })
        } else {
            // {n}
            self.expect_token(&Token::RBrace)?;
            Ok(PathQuantifier::Fixed(n))
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.5 — Insert graph pattern
    // ════════════════════════════════════════════════════════════════════════

    /// Parses an insert graph pattern: comma-separated insert path patterns.
    pub fn parse_insert_graph_pattern(&mut self) -> Result<Vec<InsertPathPattern>, GqlError> {
        self.comma_list(|p| p.parse_insert_path_pattern())
    }

    /// Parses an insert path pattern: `nodePattern (edgePattern nodePattern)*`
    fn parse_insert_path_pattern(&mut self) -> Result<InsertPathPattern, GqlError> {
        let start = self.save();
        let mut elements = vec![];

        let node = self.parse_insert_node_pattern()?;
        elements.push(InsertElement::Node(node));

        while self.at_insert_edge_start() {
            let edge = self.parse_insert_edge_pattern()?;
            elements.push(InsertElement::Edge(edge));
            let node = self.parse_insert_node_pattern()?;
            elements.push(InsertElement::Node(node));
        }

        Ok(InsertPathPattern {
            span: self.span_since(start),
            elements,
        })
    }

    /// Returns true if the current token starts an insert edge pattern.
    fn at_insert_edge_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::MinusLeftBracket | Token::LeftArrowBracket | Token::TildeLeftBracket)
        )
    }

    /// Parses an insert node pattern: `( [var] [':' labels] [{props}] )`
    fn parse_insert_node_pattern(&mut self) -> Result<InsertNodePattern, GqlError> {
        let start = self.save();
        self.expect_token(&Token::LParen)?;

        let variable = self.try_parse_element_variable();

        let mut labels = vec![];
        let is_or_colon = if self.eat_token(&Token::Colon) {
            Some(IsOrColon::Colon)
        } else if self.eat_keyword("IS") {
            Some(IsOrColon::Is)
        } else {
            None
        };
        if is_or_colon.is_some() {
            labels = self.parse_insert_label_list()?;
        }

        let properties = if self.at_token(&Token::LBrace) {
            self.parse_property_map()?
        } else {
            vec![]
        };

        self.expect_token(&Token::RParen)?;

        Ok(InsertNodePattern {
            span: self.span_since(start),
            variable,
            is_or_colon,
            labels,
            properties,
        })
    }

    /// Parses a comma-separated or `&`-separated label list for INSERT.
    fn parse_insert_label_list(&mut self) -> Result<Vec<String>, GqlError> {
        let mut labels = vec![self.expect_ident()?];
        while self.eat_token(&Token::Ampersand) {
            labels.push(self.expect_ident()?);
        }
        Ok(labels)
    }

    /// Parses an insert edge pattern (only right, left, undirected).
    fn parse_insert_edge_pattern(&mut self) -> Result<InsertEdgePattern, GqlError> {
        let start = self.save();
        let opening = self.advance().clone();
        let variable = self.try_parse_element_variable();

        let mut labels = vec![];
        let is_or_colon = if self.eat_token(&Token::Colon) {
            Some(IsOrColon::Colon)
        } else if self.eat_keyword("IS") {
            Some(IsOrColon::Is)
        } else {
            None
        };
        if is_or_colon.is_some() {
            labels = self.parse_insert_label_list()?;
        }

        let properties = if self.at_token(&Token::LBrace) {
            self.parse_property_map()?
        } else {
            vec![]
        };

        let closing = self.advance().clone();
        let direction = match (&opening, &closing) {
            (Token::MinusLeftBracket, Token::BracketRightArrow) => EdgeDirection::PointingRight,
            (Token::LeftArrowBracket, Token::RightBracketMinus) => EdgeDirection::PointingLeft,
            (Token::TildeLeftBracket, Token::RightBracketTilde) => EdgeDirection::Undirected,
            _ => {
                return Err(GqlError::Parse(format!(
                    "invalid insert edge direction: {:?} … {:?}",
                    opening, closing
                )));
            }
        };

        Ok(InsertEdgePattern {
            span: self.span_since(start),
            direction,
            variable,
            is_or_colon,
            labels,
            properties,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §16.12 — Simplified path pattern expression
    // ════════════════════════════════════════════════════════════════════════

    /// Returns true if the current token starts a simplified path pattern.
    fn at_simplified_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Token::MinusSlash
                    | Token::LeftMinusSlash
                    | Token::TildeSlash
                    | Token::LeftTildeSlash
            )
        )
    }

    /// Parses a simplified path pattern expression.
    /// Each element has a direction determined by the surrounding slash tokens.
    fn parse_simplified_path_pattern_expr(&mut self) -> Result<SimplifiedPathPattern, GqlError> {
        let start = self.save();
        let mut elements = vec![];

        let elem_start = self.save();
        let (direction, contents) = self.parse_one_simplified_element()?;
        elements.push(SimplifiedElement {
            span: self.span_since(elem_start),
            direction,
            contents,
        });

        while self.at_simplified_start() {
            let elem_start = self.save();
            let (direction, contents) = self.parse_one_simplified_element()?;
            elements.push(SimplifiedElement {
                span: self.span_since(elem_start),
                direction,
                contents,
            });
        }

        Ok(SimplifiedPathPattern {
            span: self.span_since(start),
            elements,
        })
    }

    /// Parses one simplified element: `opening_slash contents closing_slash`.
    fn parse_one_simplified_element(
        &mut self,
    ) -> Result<(EdgeDirection, SimplifiedContents), GqlError> {
        let opening = self.advance().clone();
        let contents = self.parse_simplified_contents()?;
        let closing = self.advance().clone();

        let direction = match (&opening, &closing) {
            (Token::MinusSlash, Token::SlashMinusRight) => EdgeDirection::PointingRight,
            (Token::MinusSlash, Token::SlashMinus) => EdgeDirection::AnyDirection,
            (Token::LeftMinusSlash, Token::SlashMinus) => EdgeDirection::PointingLeft,
            (Token::LeftMinusSlash, Token::SlashMinusRight) => EdgeDirection::LeftOrRight,
            (Token::TildeSlash, Token::SlashTilde) => EdgeDirection::Undirected,
            (Token::TildeSlash, Token::SlashTildeRight) => EdgeDirection::UndirectedOrRight,
            (Token::LeftTildeSlash, Token::SlashTilde) => EdgeDirection::LeftOrUndirected,
            _ => {
                return Err(GqlError::Parse(format!(
                    "invalid simplified path direction: {:?} … {:?}",
                    opening, closing
                )));
            }
        };

        Ok((direction, contents))
    }

    // ── Simplified contents recursive descent parser (GQL §16.10) ─────
    //
    // Precedence (low → high):
    //   simplifiedContents  → union (|) / multiset alternation (|+|) of terms
    //   simplifiedTerm      → concatenation (juxtaposition) of factorLows
    //   simplifiedFactorLow → conjunction (&) of factorHighs
    //   simplifiedFactorHigh→ tertiary with optional quantifier
    //   simplifiedTertiary  → direction override on secondary
    //   simplifiedSecondary → negation (!) on primary
    //   simplifiedPrimary   → labelName | (simplifiedContents)

    /// Top-level: parses union (`|`) and multiset alternation (`|+|`).
    fn parse_simplified_contents(&mut self) -> Result<SimplifiedContents, GqlError> {
        let first = self.parse_simplified_term()?;

        // Check for union or multiset alternation.
        if self.at_token(&Token::Pipe) {
            let mut left = first;
            while self.eat_token(&Token::Pipe) {
                let right = self.parse_simplified_term()?;
                left = SimplifiedContents::Union(Box::new(left), Box::new(right));
            }
            return Ok(left);
        }
        if self.at_token(&Token::MultisetAlt) {
            let mut left = first;
            while self.eat_token(&Token::MultisetAlt) {
                let right = self.parse_simplified_term()?;
                left = SimplifiedContents::MultisetAlternation(Box::new(left), Box::new(right));
            }
            return Ok(left);
        }

        Ok(first)
    }

    /// Concatenation: juxtaposition of factorLows.
    /// A new term starts when we see a token that can begin a simplified
    /// primary (identifier, `!`, `(`, `%`, direction-override tokens).
    fn parse_simplified_term(&mut self) -> Result<SimplifiedContents, GqlError> {
        let mut left = self.parse_simplified_factor_low()?;

        while self.at_simplified_primary_start() {
            let right = self.parse_simplified_factor_low()?;
            left = SimplifiedContents::Concatenation(Box::new(left), Box::new(right));
        }

        Ok(left)
    }

    /// Conjunction: `factorHigh (& factorHigh)*`.
    fn parse_simplified_factor_low(&mut self) -> Result<SimplifiedContents, GqlError> {
        let mut left = self.parse_simplified_factor_high()?;

        while self.eat_token(&Token::Ampersand) {
            let right = self.parse_simplified_factor_high()?;
            left = SimplifiedContents::Conjunction(Box::new(left), Box::new(right));
        }

        Ok(left)
    }

    /// Quantified: `tertiary [quantifier | ?]`.
    fn parse_simplified_factor_high(&mut self) -> Result<SimplifiedContents, GqlError> {
        let base = self.parse_simplified_tertiary()?;

        if let Some(q) = self.try_parse_quantifier()? {
            return Ok(SimplifiedContents::Quantified(Box::new(base), q));
        }

        Ok(base)
    }

    /// Direction override or secondary.
    ///
    /// GQL direction overrides:
    ///   `<secondary`  (left), `secondary>`  (right), `~secondary` (undirected),
    ///   `<~secondary` (left-or-undirected), `~secondary>` (undirected-or-right),
    ///   `<secondary>` (left-or-right), `-secondary` (any direction).
    ///
    /// We only handle the prefix forms (`<`, `~`, `<~`, `-`) here.
    /// The suffix `>` is checked after parsing the secondary.
    fn parse_simplified_tertiary(&mut self) -> Result<SimplifiedContents, GqlError> {
        // Prefix direction overrides.
        if self.at_token(&Token::Lt) {
            self.advance();
            // Could be `<~secondary` (left-or-undirected).
            if self.at_token(&Token::Tilde) {
                self.advance();
                let inner = self.parse_simplified_secondary()?;
                return Ok(SimplifiedContents::DirectionOverride(
                    EdgeDirection::LeftOrUndirected,
                    Box::new(inner),
                ));
            }
            let inner = self.parse_simplified_secondary()?;
            // Check for suffix `>` → `<secondary>` = left-or-right.
            if self.eat_token(&Token::Gt) {
                return Ok(SimplifiedContents::DirectionOverride(
                    EdgeDirection::LeftOrRight,
                    Box::new(inner),
                ));
            }
            return Ok(SimplifiedContents::DirectionOverride(
                EdgeDirection::PointingLeft,
                Box::new(inner),
            ));
        }
        if self.at_token(&Token::Tilde) {
            self.advance();
            let inner = self.parse_simplified_secondary()?;
            // Check for suffix `>` → `~secondary>` = undirected-or-right.
            if self.eat_token(&Token::Gt) {
                return Ok(SimplifiedContents::DirectionOverride(
                    EdgeDirection::UndirectedOrRight,
                    Box::new(inner),
                ));
            }
            return Ok(SimplifiedContents::DirectionOverride(
                EdgeDirection::Undirected,
                Box::new(inner),
            ));
        }
        // `<~` may be lexed as a single `LeftArrowTilde` token.
        if self.at_token(&Token::LeftArrowTilde) {
            self.advance();
            let inner = self.parse_simplified_secondary()?;
            return Ok(SimplifiedContents::DirectionOverride(
                EdgeDirection::LeftOrUndirected,
                Box::new(inner),
            ));
        }
        if self.at_token(&Token::Minus) {
            self.advance();
            let inner = self.parse_simplified_secondary()?;
            return Ok(SimplifiedContents::DirectionOverride(
                EdgeDirection::AnyDirection,
                Box::new(inner),
            ));
        }

        let inner = self.parse_simplified_secondary()?;
        // Suffix `>` with no prefix → `secondary>` = right override.
        if self.eat_token(&Token::Gt) {
            return Ok(SimplifiedContents::DirectionOverride(
                EdgeDirection::PointingRight,
                Box::new(inner),
            ));
        }
        Ok(inner)
    }

    /// Negation: `!primary` or just `primary`.
    fn parse_simplified_secondary(&mut self) -> Result<SimplifiedContents, GqlError> {
        if self.eat_token(&Token::Bang) {
            let inner = self.parse_simplified_primary()?;
            return Ok(SimplifiedContents::Negation(Box::new(inner)));
        }
        self.parse_simplified_primary()
    }

    /// Primary: `labelName`, `%` wildcard, or `(simplifiedContents)`.
    fn parse_simplified_primary(&mut self) -> Result<SimplifiedContents, GqlError> {
        if self.eat_token(&Token::Percent) {
            return Ok(SimplifiedContents::Label(LabelExpr::Wildcard));
        }
        if self.eat_token(&Token::LParen) {
            let inner = self.parse_simplified_contents()?;
            self.expect_token(&Token::RParen)?;
            return Ok(SimplifiedContents::Group(Box::new(inner)));
        }
        // Label name (identifier).
        let name = self.expect_ident()?;
        Ok(SimplifiedContents::Label(LabelExpr::Name(name)))
    }

    /// Returns true if the current token can start a simplified primary
    /// (used for detecting concatenation boundaries).
    fn at_simplified_primary_start(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Token::Bang
                    | Token::LParen
                    | Token::Percent
                    | Token::Lt
                    | Token::Tilde
                    | Token::Minus
                    | Token::LeftArrowTilde
            )
        ) || self.at_ident()
    }
}

// Bring Expr into scope for the where-clause parsing.
use crate::ast::Expr;
