//! Catalog / DDL statement parsers (§12): CREATE/DROP SCHEMA, GRAPH, GRAPH TYPE.

use crate::ast::{
    CreateGraphStatement, CreateGraphTypeStatement, CreateSchemaStatement, DropGraphStatement,
    DropGraphTypeStatement, DropSchemaStatement, EdgeEndpoint, EdgeTypeDef, GraphTypeDefinition,
    GraphTypeElement, GraphTypeSpec, KeyLabelSet, Keyword, NodeTypeDef, PropertyDef, Statement,
    ValueType,
};
use crate::error::GqlError;
use crate::parser::helpers::Parser;
use crate::token::{Span, Token};
use crate::types::EdgeDirection;

impl Parser<'_> {
    // ════════════════════════════════════════════════════════════════════════
    // §12 — Catalog-modifying statements
    // ════════════════════════════════════════════════════════════════════════

    /// Dispatches to the appropriate CREATE or DROP sub-parser based on
    /// lookahead keywords.
    pub fn parse_catalog_statement(&mut self) -> Result<Statement, GqlError> {
        if self.eat_keyword("CREATE") {
            self.parse_create_statement()
        } else if self.eat_keyword("DROP") {
            self.parse_drop_statement()
        } else {
            Err(self.expected("'CREATE' or 'DROP'"))
        }
    }

    // ── CREATE ──────────────────────────────────────────────────────────

    /// Parses `CREATE ...` after the CREATE keyword has been consumed.
    ///
    /// Handles:
    /// - CREATE SCHEMA
    /// - CREATE [OR REPLACE] [PROPERTY] GRAPH
    /// - CREATE [OR REPLACE] [PROPERTY] GRAPH TYPE
    fn parse_create_statement(&mut self) -> Result<Statement, GqlError> {
        // CREATE OR REPLACE ...
        let or_replace = if self.at_keyword("OR") && self.at_keyword_ahead(1, "REPLACE") {
            self.advance(); // OR
            self.advance(); // REPLACE
            true
        } else {
            false
        };

        // CREATE SCHEMA ...
        if !or_replace && self.at_keyword("SCHEMA") {
            return self.parse_create_schema();
        }

        // Consume optional PROPERTY keyword.
        let property_keyword = self.eat_keyword("PROPERTY");

        // CREATE [OR REPLACE] [PROPERTY] GRAPH TYPE ...
        if self.at_keyword("GRAPH") && self.at_keyword_ahead(1, "TYPE") {
            return self.parse_create_graph_type(or_replace, property_keyword);
        }

        // CREATE [OR REPLACE] [PROPERTY] GRAPH ...
        if self.at_keyword("GRAPH") {
            return self.parse_create_graph(or_replace, property_keyword);
        }

        Err(self.expected("'SCHEMA', 'GRAPH', or 'GRAPH TYPE' after CREATE"))
    }

    /// Parses `CREATE SCHEMA [IF NOT EXISTS] <name>`.
    fn parse_create_schema(&mut self) -> Result<Statement, GqlError> {
        let start = self.save();
        self.expect_keyword("SCHEMA")?;
        let if_not_exists = self.parse_if_not_exists()?;
        let name = self.parse_schema_name()?;
        Ok(Statement::CreateSchema(CreateSchemaStatement {
            span: self.span_since(start),
            if_not_exists,
            name,
        }))
    }

    /// Parses `CREATE [OR REPLACE] [PROPERTY] GRAPH [IF NOT EXISTS] <name> ...`.
    fn parse_create_graph(
        &mut self,
        or_replace: bool,
        property_keyword: bool,
    ) -> Result<Statement, GqlError> {
        let start = self.save();
        self.expect_keyword("GRAPH")?;
        let if_not_exists = self.parse_if_not_exists()?;
        let name = self.parse_object_name()?;

        // Graph type specification (optional).
        let graph_type = self.parse_optional_graph_type_spec()?;

        // Optional: AS COPY OF <source>.
        let copy_of = if self.at_keyword("AS")
            && self.at_keyword_ahead(1, "COPY")
            && self.at_keyword_ahead(2, "OF")
        {
            self.advance(); // AS
            self.advance(); // COPY
            self.advance(); // OF
            Some(self.parse_object_name()?)
        } else {
            None
        };

        Ok(Statement::CreateGraph(CreateGraphStatement {
            span: self.span_since(start),
            property_keyword,
            or_replace,
            if_not_exists,
            name,
            graph_type,
            copy_of,
        }))
    }

    /// Parses `CREATE [OR REPLACE] [PROPERTY] GRAPH TYPE [IF NOT EXISTS] <name> <source>`.
    fn parse_create_graph_type(
        &mut self,
        or_replace: bool,
        property_keyword: bool,
    ) -> Result<Statement, GqlError> {
        let start = self.save();
        self.expect_keyword("GRAPH")?;
        self.expect_keyword("TYPE")?;
        let if_not_exists = self.parse_if_not_exists()?;
        let name = self.parse_object_name()?;

        // [AS] COPY OF <source-type>
        let (as_keyword, copy_of) = if self.at_keyword("COPY")
            || (self.at_keyword("AS")
                && self.at_keyword_ahead(1, "COPY")
                && self.at_keyword_ahead(2, "OF"))
        {
            let as_kw = self.eat_keyword("AS");
            self.expect_keyword("COPY")?;
            self.expect_keyword("OF")?;
            (as_kw, Some(self.parse_object_name()?))
        } else {
            (false, None)
        };

        // The graph type definition body.
        let definition = self.parse_graph_type_definition()?;

        Ok(Statement::CreateGraphType(CreateGraphTypeStatement {
            span: self.span_since(start),
            property_keyword,
            or_replace,
            if_not_exists,
            name,
            definition,
            as_keyword,
            copy_of,
        }))
    }

    // ── DROP ────────────────────────────────────────────────────────────

    /// Parses `DROP ...` after the DROP keyword has been consumed.
    fn parse_drop_statement(&mut self) -> Result<Statement, GqlError> {
        // DROP SCHEMA [IF EXISTS] <name>
        if self.at_keyword("SCHEMA") {
            return self.parse_drop_schema();
        }

        // Consume optional PROPERTY keyword.
        let property_keyword = self.eat_keyword("PROPERTY");

        // DROP [PROPERTY] GRAPH TYPE [IF EXISTS] <name>
        if self.at_keyword("GRAPH") && self.at_keyword_ahead(1, "TYPE") {
            return self.parse_drop_graph_type(property_keyword);
        }

        // DROP [PROPERTY] GRAPH [IF EXISTS] <name>
        if self.at_keyword("GRAPH") {
            return self.parse_drop_graph(property_keyword);
        }

        Err(self.expected("'SCHEMA', 'GRAPH', or 'GRAPH TYPE' after DROP"))
    }

    /// Parses `DROP SCHEMA [IF EXISTS] <name>`.
    fn parse_drop_schema(&mut self) -> Result<Statement, GqlError> {
        let start = self.save();
        self.expect_keyword("SCHEMA")?;
        let if_exists = self.parse_if_exists()?;
        let name = self.parse_schema_name()?;
        Ok(Statement::DropSchema(DropSchemaStatement {
            span: self.span_since(start),
            if_exists,
            name,
        }))
    }

    /// Parses `DROP [PROPERTY] GRAPH [IF EXISTS] <name>`.
    fn parse_drop_graph(&mut self, property_keyword: bool) -> Result<Statement, GqlError> {
        let start = self.save();
        self.expect_keyword("GRAPH")?;
        let if_exists = self.parse_if_exists()?;
        let name = self.parse_object_name()?;
        Ok(Statement::DropGraph(DropGraphStatement {
            span: self.span_since(start),
            property_keyword,
            if_exists,
            name,
        }))
    }

    /// Parses `DROP [PROPERTY] GRAPH TYPE [IF EXISTS] <name>`.
    fn parse_drop_graph_type(&mut self, property_keyword: bool) -> Result<Statement, GqlError> {
        let start = self.save();
        self.expect_keyword("GRAPH")?;
        self.expect_keyword("TYPE")?;
        let if_exists = self.parse_if_exists()?;
        let name = self.parse_object_name()?;
        Ok(Statement::DropGraphType(DropGraphTypeStatement {
            span: self.span_since(start),
            property_keyword,
            if_exists,
            name,
        }))
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Parses optional `IF NOT EXISTS`, returning `true` if present.
    fn parse_if_not_exists(&mut self) -> Result<bool, GqlError> {
        if self.at_keyword("IF")
            && self.at_keyword_ahead(1, "NOT")
            && self.at_keyword_ahead(2, "EXISTS")
        {
            self.advance(); // IF
            self.advance(); // NOT
            self.advance(); // EXISTS
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Parses optional `IF EXISTS`, returning `true` if present.
    fn parse_if_exists(&mut self) -> Result<bool, GqlError> {
        if self.at_keyword("IF") && self.at_keyword_ahead(1, "EXISTS") {
            self.advance(); // IF
            self.advance(); // EXISTS
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Parses an optional graph type specification after a graph name.
    ///
    /// - `LIKE <name>` — reference another graph's type
    /// - `TYPED <name>` / `:: <name>` — explicit type reference
    /// - `{ ... }` — inline graph type definition
    /// - `ANY` — open graph type (returns `None`)
    fn parse_optional_graph_type_spec(&mut self) -> Result<Option<GraphTypeSpec>, GqlError> {
        if self.eat_keyword("LIKE") {
            let name = self.parse_object_name()?;
            return Ok(Some(GraphTypeSpec::Like(name)));
        }

        if self.at_keyword("TYPED") || self.at_token(&Token::DoubleColon) {
            let typed_keyword = self.eat_keyword("TYPED");
            if !typed_keyword {
                self.advance(); // consume ::
            }
            // Could be an inline definition or a named reference.
            if self.at_token(&Token::LBrace) {
                let def = self.parse_graph_type_definition()?;
                return Ok(Some(GraphTypeSpec::Inline(def)));
            }
            let name = self.parse_object_name()?;
            return Ok(Some(GraphTypeSpec::Typed {
                name,
                typed_keyword,
            }));
        }

        if self.at_token(&Token::LBrace) {
            let def = self.parse_graph_type_definition()?;
            return Ok(Some(GraphTypeSpec::Inline(def)));
        }

        // ANY [PROPERTY] GRAPH — open graph type.
        if self.eat_keyword("ANY") {
            let property_keyword = self.eat_keyword("PROPERTY");
            let graph_keyword = self.eat_keyword("GRAPH");
            return Ok(Some(GraphTypeSpec::Any {
                property_keyword,
                graph_keyword,
            }));
        }

        // Bare identifier — implicit graph type reference (GQL §12.3 ofGraphType).
        // Only treat as type ref if the next token is an identifier that is NOT a
        // statement-starting keyword (AS, NEXT, etc.).
        if self.at_ident() && !self.at_keyword("AS") && !self.at_keyword("NEXT") {
            let name = self.parse_object_name()?;
            return Ok(Some(GraphTypeSpec::Typed {
                name,
                typed_keyword: false,
            }));
        }

        Ok(None)
    }

    /// Parses a graph type definition body: `{ element, ... }`.
    fn parse_graph_type_definition(&mut self) -> Result<GraphTypeDefinition, GqlError> {
        let start = self.save();
        if !self.eat_token(&Token::LBrace) {
            return Ok(GraphTypeDefinition {
                span: self.span_since(start),
                elements: Vec::new(),
            });
        }

        let mut elements = Vec::new();
        if self.eat_token(&Token::RBrace) {
            return Ok(GraphTypeDefinition {
                span: self.span_since(start),
                elements,
            });
        }

        loop {
            elements.push(self.parse_graph_type_element()?);
            if self.eat_token(&Token::Comma) {
                continue;
            }
            break;
        }

        self.expect_token(&Token::RBrace)?;
        Ok(GraphTypeDefinition {
            span: self.span_since(start),
            elements,
        })
    }

    fn parse_graph_type_element(&mut self) -> Result<GraphTypeElement, GqlError> {
        if self.at_keyword("NODE") || self.at_keyword("VERTEX") {
            return Ok(GraphTypeElement::Node(self.parse_phrase_node_type_def()?));
        }
        if self.at_keyword("DIRECTED") || self.at_keyword("UNDIRECTED") {
            return Ok(GraphTypeElement::Edge(self.parse_phrase_edge_type_def()?));
        }
        if self.at_token(&Token::LParen) {
            return self.parse_pattern_graph_type_element();
        }
        Err(self.expected("graph type element"))
    }

    fn parse_pattern_graph_type_element(&mut self) -> Result<GraphTypeElement, GqlError> {
        let start = self.save();
        let left = self.parse_pattern_node_reference()?;
        if matches!(
            self.peek(),
            Some(Token::MinusLeftBracket | Token::TildeLeftBracket | Token::LeftArrowBracket)
        ) {
            let edge = self.parse_pattern_edge_type_def(left)?;
            return Ok(GraphTypeElement::Edge(edge));
        }

        Ok(GraphTypeElement::Node(NodeTypeDef {
            span: self.span_since(start),
            keyword: Keyword::new("NODE"),
            name: left.type_name,
            alias: None,
            label_set: left.label.map(|l| KeyLabelSet {
                span: self.span_since(start),
                label_keyword_plural: false,
                labels: vec![l],
            }),
            properties: left.properties,
        }))
    }

    fn parse_pattern_node_reference(&mut self) -> Result<PatternNodeTypeRef, GqlError> {
        self.expect_token(&Token::LParen)?;
        let name = if self.at_ident() && !self.at_token(&Token::Colon) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        let label = if self.eat_token(&Token::Colon) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        let properties = if self.at_token(&Token::LBrace) {
            self.parse_graph_type_property_defs()?
        } else {
            Vec::new()
        };
        self.expect_token(&Token::RParen)?;
        Ok(PatternNodeTypeRef {
            type_name: name,
            label,
            properties,
        })
    }

    fn parse_pattern_edge_type_def(
        &mut self,
        left: PatternNodeTypeRef,
    ) -> Result<EdgeTypeDef, GqlError> {
        let start = self.save();
        let direction = match self.advance() {
            Token::MinusLeftBracket => EdgeDirection::PointingRight,
            Token::TildeLeftBracket => EdgeDirection::Undirected,
            Token::LeftArrowBracket => EdgeDirection::PointingLeft,
            _ => return Err(self.expected("graph type edge connector")),
        };

        let name =
            if self.at_ident() && !self.at_token(&Token::Colon) && !self.at_token(&Token::RBracket)
            {
                Some(self.expect_ident()?)
            } else {
                None
            };
        let label_set = if self.eat_token(&Token::Colon) {
            Some(self.parse_graph_type_labels(false)?)
        } else {
            None
        };
        match direction {
            EdgeDirection::PointingRight => self.expect_token(&Token::BracketRightArrow)?,
            EdgeDirection::Undirected => self.expect_token(&Token::RightBracketTilde)?,
            EdgeDirection::PointingLeft => self.expect_token(&Token::RightBracketMinus)?,
            _ => return Err(self.expected("supported graph type edge direction")),
        }

        let right = self.parse_pattern_node_reference()?;
        let (source, destination) = match direction {
            EdgeDirection::PointingLeft => (
                Self::edge_endpoint_from_pattern_ref(&right),
                Self::edge_endpoint_from_pattern_ref(&left),
            ),
            _ => (
                Self::edge_endpoint_from_pattern_ref(&left),
                Self::edge_endpoint_from_pattern_ref(&right),
            ),
        };

        Ok(EdgeTypeDef {
            span: self.span_since(start),
            keyword: Keyword::new("EDGE"),
            name,
            direction,
            source,
            destination,
            label_set,
            properties: Vec::new(),
        })
    }

    fn parse_phrase_node_type_def(&mut self) -> Result<NodeTypeDef, GqlError> {
        let start = self.save();
        let keyword = Keyword::new(self.current_ident_upper());
        if !self.eat_keyword("NODE") {
            self.expect_keyword("VERTEX")?;
        }
        let name = Some(self.expect_ident()?);
        let label_set = if self.at_keyword("LABELS") || self.at_keyword("LABEL") {
            let plural = self.eat_keyword("LABELS");
            if !plural {
                self.advance();
            } // consume LABEL
            Some(self.parse_graph_type_labels(plural)?)
        } else {
            None
        };
        let properties = if self.at_token(&Token::LBrace) {
            self.parse_graph_type_property_defs()?
        } else {
            Vec::new()
        };
        let alias = if self.eat_keyword("AS") {
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(NodeTypeDef {
            span: self.span_since(start),
            keyword,
            name,
            alias,
            label_set,
            properties,
        })
    }

    fn parse_phrase_edge_type_def(&mut self) -> Result<EdgeTypeDef, GqlError> {
        let start = self.save();
        let direction = if self.eat_keyword("DIRECTED") {
            EdgeDirection::PointingRight
        } else if self.eat_keyword("UNDIRECTED") {
            EdgeDirection::Undirected
        } else {
            return Err(self.expected("'DIRECTED' or 'UNDIRECTED'"));
        };
        let keyword = Keyword::new(self.current_ident_upper());
        if !self.eat_keyword("EDGE") {
            self.expect_keyword("RELATIONSHIP")?;
        }
        let name = Some(self.expect_ident()?);
        let label_set = if self.at_keyword("LABELS") || self.at_keyword("LABEL") {
            let plural = self.eat_keyword("LABELS");
            if !plural {
                self.advance();
            } // consume LABEL
            Some(self.parse_graph_type_labels(plural)?)
        } else {
            None
        };
        let properties = if self.at_token(&Token::LBrace) {
            self.parse_graph_type_property_defs()?
        } else {
            Vec::new()
        };
        self.expect_keyword("CONNECTING")?;
        let (source, destination) = self.parse_endpoint_pair(direction)?;
        Ok(EdgeTypeDef {
            span: self.span_since(start),
            keyword,
            name,
            direction,
            source,
            destination,
            label_set,
            properties,
        })
    }

    fn parse_endpoint_pair(
        &mut self,
        direction: EdgeDirection,
    ) -> Result<(EdgeEndpoint, EdgeEndpoint), GqlError> {
        let start = self.save();
        self.expect_token(&Token::LParen)?;
        let left = EdgeEndpoint {
            span: self.span_since(start),
            label: None,
            type_name: Some(self.expect_ident()?),
        };
        let pair_direction = if self.eat_token(&Token::RightArrow) {
            EdgeDirection::PointingRight
        } else if self.eat_token(&Token::LeftArrow) {
            EdgeDirection::PointingLeft
        } else if self.eat_token(&Token::Tilde) {
            EdgeDirection::Undirected
        } else {
            return Err(self.expected("endpoint connector"));
        };
        let start_right = self.save();
        let right = EdgeEndpoint {
            span: self.span_since(start_right),
            label: None,
            type_name: Some(self.expect_ident()?),
        };
        self.expect_token(&Token::RParen)?;

        match pair_direction {
            EdgeDirection::PointingLeft => Ok((right, left)),
            EdgeDirection::PointingRight | EdgeDirection::Undirected => Ok((left, right)),
            _ => Err(self.error(format!(
                "unsupported graph type endpoint direction: {direction:?}"
            ))),
        }
    }

    fn parse_graph_type_labels(
        &mut self,
        label_keyword_plural: bool,
    ) -> Result<KeyLabelSet, GqlError> {
        let start = self.save();
        let mut labels = vec![self.expect_ident()?];
        while self.eat_token(&Token::Ampersand) {
            labels.push(self.expect_ident()?);
        }
        Ok(KeyLabelSet {
            span: self.span_since(start),
            label_keyword_plural,
            labels,
        })
    }

    fn parse_graph_type_property_defs(&mut self) -> Result<Vec<PropertyDef>, GqlError> {
        self.expect_token(&Token::LBrace)?;
        let mut properties = Vec::new();
        if self.eat_token(&Token::RBrace) {
            return Ok(properties);
        }
        loop {
            properties.push(self.parse_graph_type_property_def()?);
            if self.eat_token(&Token::Comma) {
                continue;
            }
            break;
        }
        self.expect_token(&Token::RBrace)?;
        Ok(properties)
    }

    fn parse_graph_type_property_def(&mut self) -> Result<PropertyDef, GqlError> {
        let start = self.save();
        let name = self.expect_ident()?;
        let value_type = self.parse_value_type()?;
        let (value_type, not_null) = match value_type {
            ValueType::NotNull(inner) => (*inner, true),
            other => (other, false),
        };
        Ok(PropertyDef {
            span: self.span_since(start),
            name,
            value_type,
            not_null,
            default_value: None,
        })
    }

    fn edge_endpoint_from_pattern_ref(node: &PatternNodeTypeRef) -> EdgeEndpoint {
        EdgeEndpoint {
            span: Span::DUMMY,
            label: node.label.clone(),
            type_name: node.type_name.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct PatternNodeTypeRef {
    type_name: Option<String>,
    label: Option<String>,
    properties: Vec<PropertyDef>,
}
