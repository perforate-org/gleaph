//! Session and transaction command parsers (§7–§8).

use crate::ast::{
    ProcedureBindingKind, SessionCommand, SessionResetCommand, SessionSetCommand,
    StartTransactionCommand, TransactionAccessMode,
};
use crate::error::GqlError;
use crate::parser::helpers::Parser;

impl Parser<'_> {
    // ════════════════════════════════════════════════════════════════════════
    // §7.1–7.3 — Session commands
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a session command: `SESSION (SET | RESET | CLOSE) ...`.
    pub fn parse_session_command(&mut self) -> Result<SessionCommand, GqlError> {
        self.expect_keyword("SESSION")?;

        if self.eat_keyword("SET") {
            let cmd = self.parse_session_set()?;
            Ok(SessionCommand::Set(cmd))
        } else if self.eat_keyword("RESET") {
            let cmd = self.parse_session_reset()?;
            Ok(SessionCommand::Reset(cmd))
        } else if self.eat_keyword("CLOSE") {
            Ok(SessionCommand::Close)
        } else {
            Err(self.expected("'SET', 'RESET', or 'CLOSE' after SESSION"))
        }
    }

    /// Parses the body of `SESSION SET ...`.
    ///
    /// GQL §7.1 sessionSetCommand forms:
    /// - `SESSION SET SCHEMA <name>`
    /// - `SESSION SET [PROPERTY] GRAPH <expr>`
    /// - `SESSION SET TIME ZONE <value>`
    /// - `SESSION SET VALUE [IF NOT EXISTS] $name = <value>`
    fn parse_session_set(&mut self) -> Result<SessionSetCommand, GqlError> {
        // Helper for parsing `IF NOT EXISTS` in sessionSetParameterName.
        fn eat_if_not_exists(p: &mut Parser<'_>) -> bool {
            if p.at_keyword("IF") && p.at_keyword_ahead(1, "NOT") && p.at_keyword_ahead(2, "EXISTS")
            {
                p.advance(); // IF
                p.advance(); // NOT
                p.advance(); // EXISTS
                true
            } else {
                false
            }
        }

        if self.eat_keyword("SCHEMA") {
            let name = self.parse_schema_name()?;
            Ok(SessionSetCommand::Schema(name))
        } else if self.at_keyword("GRAPH") || self.at_keyword("PROPERTY") {
            // SESSION SET [PROPERTY] GRAPH ...
            let property_keyword = self.eat_keyword("PROPERTY");
            self.expect_keyword("GRAPH")?;
            // Check if this is a graph parameter: SESSION SET GRAPH [IF NOT EXISTS] $param = expr
            let if_not_exists = eat_if_not_exists(self);
            if let Some(crate::token::Token::Param(_)) = self.peek() {
                let param_name = if let Some(crate::token::Token::Param(p)) = self.peek().cloned() {
                    self.advance();
                    p
                } else {
                    unreachable!()
                };
                let (typed_prefix, type_annotation) =
                    self.parse_binding_type_annotation(&ProcedureBindingKind::Graph)?;
                self.expect_token(&crate::token::Token::Eq)?;
                let value = self.parse_expr()?;
                return Ok(SessionSetCommand::GraphParameter {
                    property_keyword,
                    if_not_exists,
                    name: param_name,
                    typed_prefix,
                    type_annotation,
                    value: Box::new(value),
                });
            }
            let name = self.parse_object_name()?;
            Ok(SessionSetCommand::Graph {
                property_keyword,
                name,
            })
        } else if self.at_keyword("BINDING") || self.at_keyword("TABLE") {
            // SESSION SET [BINDING] TABLE [IF NOT EXISTS] $param = expr
            let binding_keyword = self.eat_keyword("BINDING");
            self.expect_keyword("TABLE")?;
            let if_not_exists = eat_if_not_exists(self);
            let name = if let Some(crate::token::Token::Param(p)) = self.peek().cloned() {
                self.advance();
                p
            } else {
                return Err(self.expected("parameter name ($name) after TABLE"));
            };
            let (typed_prefix, type_annotation) =
                self.parse_binding_type_annotation(&ProcedureBindingKind::Table)?;
            self.expect_token(&crate::token::Token::Eq)?;
            let value = self.parse_expr()?;
            Ok(SessionSetCommand::BindingTableParameter {
                binding_keyword,
                if_not_exists,
                name,
                typed_prefix,
                type_annotation,
                value: Box::new(value),
            })
        } else if self.at_keyword("TIME") && self.at_keyword_ahead(1, "ZONE") {
            self.advance(); // TIME
            self.advance(); // ZONE
            let value = self.parse_expr()?;
            Ok(SessionSetCommand::TimeZone(Box::new(value)))
        } else if self.eat_keyword("VALUE") {
            // SESSION SET VALUE [IF NOT EXISTS] $name = <value>
            // GQL §7.1: sessionSetValueParameterClause
            let if_not_exists = eat_if_not_exists(self);

            // Parameter name: $name (GENERAL_PARAMETER_REFERENCE).
            let name = if let Some(crate::token::Token::Param(p)) = self.peek().cloned() {
                self.advance();
                p
            } else {
                return Err(self.expected("parameter name ($name) after VALUE"));
            };

            let (typed_prefix, type_annotation) =
                self.parse_binding_type_annotation(&ProcedureBindingKind::Value)?;
            self.expect_token(&crate::token::Token::Eq)?;
            let value = self.parse_expr()?;
            Ok(SessionSetCommand::Parameter {
                if_not_exists,
                name,
                typed_prefix,
                type_annotation,
                value: Box::new(value),
            })
        } else {
            Err(self
                .expected("'SCHEMA', 'GRAPH', 'TABLE', 'TIME ZONE', or 'VALUE' after SESSION SET"))
        }
    }

    /// Parses the body of `SESSION RESET ...`.
    ///
    /// GQL §7.2 sessionResetArguments:
    /// - `SESSION RESET` (no arguments — reset all)
    /// - `SESSION RESET ALL? (PARAMETERS | CHARACTERISTICS)`
    /// - `SESSION RESET SCHEMA`
    /// - `SESSION RESET [PROPERTY] GRAPH`
    /// - `SESSION RESET TIME ZONE`
    /// - `SESSION RESET PARAMETER? $name`
    fn parse_session_reset(&mut self) -> Result<SessionResetCommand, GqlError> {
        // SESSION RESET with no arguments — peek ahead to see if we're at
        // end of input or at a keyword that starts a new statement.
        if self.at_end() {
            return Ok(SessionResetCommand::All);
        }

        if self.eat_keyword("SCHEMA") {
            return Ok(SessionResetCommand::Schema);
        }

        if self.at_keyword("GRAPH") || self.at_keyword("PROPERTY") {
            let property_keyword = self.eat_keyword("PROPERTY");
            self.expect_keyword("GRAPH")?;
            return Ok(SessionResetCommand::Graph { property_keyword });
        }

        if self.at_keyword("TIME") && self.at_keyword_ahead(1, "ZONE") {
            self.advance(); // TIME
            self.advance(); // ZONE
            return Ok(SessionResetCommand::TimeZone);
        }

        // ALL PARAMETERS | ALL CHARACTERISTICS
        if self.eat_keyword("ALL") {
            if self.eat_keyword("PARAMETERS") {
                return Ok(SessionResetCommand::AllParameters { all_keyword: true });
            } else if self.eat_keyword("CHARACTERISTICS") {
                return Ok(SessionResetCommand::AllCharacteristics { all_keyword: true });
            }
            return Err(self.expected("'PARAMETERS' or 'CHARACTERISTICS' after ALL"));
        }

        // PARAMETERS | CHARACTERISTICS (without ALL)
        if self.eat_keyword("PARAMETERS") {
            return Ok(SessionResetCommand::AllParameters { all_keyword: false });
        }
        if self.eat_keyword("CHARACTERISTICS") {
            return Ok(SessionResetCommand::AllCharacteristics { all_keyword: false });
        }

        // PARAMETER? $name (sessionParameterSpecification = GENERAL_PARAMETER_REFERENCE)
        let parameter_keyword = self.eat_keyword("PARAMETER"); // optional
        if let Some(crate::token::Token::Param(p)) = self.peek().cloned() {
            self.advance();
            return Ok(SessionResetCommand::Parameter {
                parameter_keyword,
                name: p,
            });
        }

        Err(self.expected(
            "'SCHEMA', 'GRAPH', 'TIME ZONE', 'ALL PARAMETERS', 'PARAMETER $name', or '$name' after SESSION RESET",
        ))
    }

    // ════════════════════════════════════════════════════════════════════════
    // §8.1 — START TRANSACTION
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `START TRANSACTION [READ ONLY | READ WRITE [, ...]]`.
    pub fn parse_start_transaction(&mut self) -> Result<StartTransactionCommand, GqlError> {
        let start = self.save();
        self.expect_keyword("START")?;
        self.expect_keyword("TRANSACTION")?;

        let mut access_modes = Vec::new();
        if self.eat_keyword("READ") {
            if self.eat_keyword("ONLY") {
                access_modes.push(TransactionAccessMode::ReadOnly);
            } else if self.eat_keyword("WRITE") {
                access_modes.push(TransactionAccessMode::ReadWrite);
            } else {
                return Err(self.expected("'ONLY' or 'WRITE' after READ"));
            }

            // Additional comma-separated characteristics.
            while self.eat_token(&crate::token::Token::Comma) {
                self.expect_keyword("READ")?;
                if self.eat_keyword("ONLY") {
                    access_modes.push(TransactionAccessMode::ReadOnly);
                } else if self.eat_keyword("WRITE") {
                    access_modes.push(TransactionAccessMode::ReadWrite);
                } else {
                    return Err(self.expected("'ONLY' or 'WRITE' after READ"));
                }
            }
        }

        Ok(StartTransactionCommand {
            span: self.span_since(start),
            access_modes,
        })
    }
}
