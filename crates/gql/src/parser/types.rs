//! Value type parser (GQL section 18.9).

use crate::ast::{Keyword, RecordFieldType, TypedPrefix, ValueType};
use crate::error::GqlError;
use crate::token::Token;

use super::helpers::Parser;

impl Parser<'_> {
    /// Parses a GQL value type (section 18.9).
    ///
    /// This handles all predefined types, constructed types (list, record, path),
    /// dynamic union types, and closed dynamic union types (`T | U`).
    pub fn parse_value_type(&mut self) -> Result<ValueType, GqlError> {
        let mut ty = self.parse_value_type_primary()?;

        // Handle postfix LIST/ARRAY: `valueType LIST [max_len]`
        while self.at_keyword("LIST") || self.at_keyword("ARRAY") {
            let list_kw = self.current_ident_upper();
            self.advance(); // consume LIST/ARRAY
            let max_length = self.try_parse_bracket_length()?;
            let not_null = self.eat_not_null();
            ty = ValueType::List {
                keyword: Keyword::new(list_kw),
                element_type: Box::new(ty),
                max_length,
            };
            if not_null {
                ty = ValueType::NotNull(Box::new(ty));
            }
        }

        // Handle closed dynamic union: `type | type`
        if self.at_token(&Token::Pipe) {
            let mut members = vec![ty];
            while self.eat_token(&Token::Pipe) {
                let mut rhs = self.parse_value_type_primary()?;
                // Handle postfix LIST/ARRAY on the rhs as well
                while self.at_keyword("LIST") || self.at_keyword("ARRAY") {
                    let list_kw = self.current_ident_upper();
                    self.advance();
                    let max_length = self.try_parse_bracket_length()?;
                    let not_null = self.eat_not_null();
                    rhs = ValueType::List {
                        keyword: Keyword::new(list_kw),
                        element_type: Box::new(rhs),
                        max_length,
                    };
                    if not_null {
                        rhs = ValueType::NotNull(Box::new(rhs));
                    }
                }
                members.push(rhs);
            }
            ty = ValueType::ClosedDynamicUnion(members);
        }

        Ok(ty)
    }

    /// Parses a primary (non-union, non-postfix-list) value type.
    fn parse_value_type_primary(&mut self) -> Result<ValueType, GqlError> {
        self.recurse(Self::parse_value_type_primary_inner)
    }

    fn parse_value_type_primary_inner(&mut self) -> Result<ValueType, GqlError> {
        match self.peek() {
            // ── Boolean ────────────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "BOOL") || kw_eq(s, "BOOLEAN") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::Bool {
                    keyword: Keyword::new(kw),
                }))
            }

            // ── String types ───────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "STRING") => {
                self.advance();
                let (min_length, max_length) = if self.eat_token(&Token::LParen) {
                    let first = self.expect_unsigned_int()?;
                    if self.eat_token(&Token::Comma) {
                        // STRING(min, max)
                        let max = self.expect_unsigned_int()?;
                        self.expect_token(&Token::RParen)?;
                        (Some(first), Some(max))
                    } else {
                        // STRING(max)
                        self.expect_token(&Token::RParen)?;
                        (None, Some(first))
                    }
                } else {
                    (None, None)
                };
                Ok(self.wrap_not_null(ValueType::String {
                    min_length,
                    max_length,
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "CHAR") || kw_eq(s, "CHARACTER") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                // CHARACTER VARYING => VARCHAR
                if self.eat_keyword("VARYING") {
                    let varchar_kw = if kw == "CHARACTER" {
                        "CHARACTER VARYING"
                    } else {
                        "CHAR VARYING"
                    };
                    let max_length = self.try_parse_paren_single()?;
                    return Ok(self.wrap_not_null(ValueType::Varchar {
                        keyword: Keyword::new(varchar_kw),
                        max_length,
                    }));
                }
                let length = self.try_parse_paren_single()?;
                Ok(self.wrap_not_null(ValueType::Char {
                    keyword: Keyword::new(kw),
                    length,
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "VARCHAR") => {
                self.advance();
                let max_length = self.try_parse_paren_single()?;
                Ok(self.wrap_not_null(ValueType::Varchar {
                    keyword: Keyword::new("VARCHAR"),
                    max_length,
                }))
            }

            // ── Byte string types ──────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "BYTES") => {
                self.advance();
                let max_length = if self.eat_token(&Token::LParen) {
                    let first = self.expect_unsigned_int()?;

                    if self.eat_token(&Token::Comma) {
                        let m = self.expect_unsigned_int()?;
                        self.expect_token(&Token::RParen)?;
                        Some(m)
                    } else {
                        self.expect_token(&Token::RParen)?;
                        Some(first)
                    }
                } else {
                    None
                };
                Ok(self.wrap_not_null(ValueType::Bytes { max_length }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "BINARY") => {
                self.advance();
                // BINARY VARYING => VARBINARY
                if self.eat_keyword("VARYING") {
                    let max_length = self.try_parse_paren_single()?;
                    return Ok(self.wrap_not_null(ValueType::Varbinary {
                        keyword: Keyword::new("BINARY VARYING"),
                        max_length,
                    }));
                }
                let length = self.try_parse_paren_single()?;
                Ok(self.wrap_not_null(ValueType::Binary { length }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "VARBINARY") => {
                self.advance();
                let max_length = self.try_parse_paren_single()?;
                Ok(self.wrap_not_null(ValueType::Varbinary {
                    keyword: Keyword::new("VARBINARY"),
                    max_length,
                }))
            }

            // ── Signed integer types ───────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "INT8") || kw_eq(s, "INTEGER8") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int8 {
                    keyword: Keyword::new(kw),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "INT16") || kw_eq(s, "INTEGER16") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int16 {
                    keyword: Keyword::new(kw),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "INT32") || kw_eq(s, "INTEGER32") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int32 {
                    keyword: Keyword::new(kw),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "INT64") || kw_eq(s, "INTEGER64") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int64 {
                    keyword: Keyword::new(kw),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "INT128") || kw_eq(s, "INTEGER128") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int128 {
                    keyword: Keyword::new(kw),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "INT256") || kw_eq(s, "INTEGER256") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int256 {
                    keyword: Keyword::new(kw),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "SMALLINT") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int16 {
                    keyword: Keyword::new("SMALLINT"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "BIGINT") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int64 {
                    keyword: Keyword::new("BIGINT"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "INT") => {
                self.advance();
                let precision = self.try_parse_paren_single()?;
                let ty = match precision {
                    Some(p) => ValueType::IntPrecision {
                        keyword: Keyword::new("INT"),
                        precision: p,
                    },
                    None => ValueType::Int32 {
                        keyword: Keyword::new("INT"),
                    },
                };
                Ok(self.wrap_not_null(ty))
            }
            Some(Token::Ident(s)) if kw_eq(s, "INTEGER") => {
                self.advance();
                let precision = self.try_parse_paren_single()?;
                let ty = match precision {
                    Some(p) => ValueType::IntPrecision {
                        keyword: Keyword::new("INTEGER"),
                        precision: p,
                    },
                    None => ValueType::Int32 {
                        keyword: Keyword::new("INTEGER"),
                    },
                };
                Ok(self.wrap_not_null(ty))
            }
            Some(Token::Ident(s)) if kw_eq(s, "SIGNED") => {
                self.advance();
                self.parse_verbose_signed_integer()
            }

            // ── Unsigned integer types ─────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "UINT8") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Uint8 {
                    keyword: Keyword::new("UINT8"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "UINT16") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Uint16 {
                    keyword: Keyword::new("UINT16"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "UINT32") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Uint32 {
                    keyword: Keyword::new("UINT32"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "UINT64") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Uint64 {
                    keyword: Keyword::new("UINT64"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "UINT128") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Uint128 {
                    keyword: Keyword::new("UINT128"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "UINT256") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Uint256 {
                    keyword: Keyword::new("UINT256"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "USMALLINT") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Uint16 {
                    keyword: Keyword::new("USMALLINT"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "UBIGINT") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Uint64 {
                    keyword: Keyword::new("UBIGINT"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "UINT") => {
                self.advance();
                let precision = self.try_parse_paren_single()?;
                let ty = match precision {
                    Some(p) => ValueType::UintPrecision {
                        keyword: Keyword::new("UINT"),
                        precision: p,
                    },
                    None => ValueType::Uint32 {
                        keyword: Keyword::new("UINT"),
                    },
                };
                Ok(self.wrap_not_null(ty))
            }
            Some(Token::Ident(s)) if kw_eq(s, "UNSIGNED") => {
                self.advance();
                self.parse_verbose_unsigned_integer()
            }

            // ── Decimal types ──────────────────────────────────────────────
            Some(Token::Ident(s))
                if kw_eq(s, "DECIMAL") || kw_eq(s, "DEC") || kw_eq(s, "NUMERIC") =>
            {
                let kw = s.to_ascii_uppercase();
                self.advance();
                let (precision, scale) = if self.eat_token(&Token::LParen) {
                    let p = self.expect_unsigned_int()?;
                    let s = if self.eat_token(&Token::Comma) {
                        Some(self.expect_unsigned_int()?)
                    } else {
                        None
                    };
                    self.expect_token(&Token::RParen)?;
                    (Some(p), s)
                } else {
                    (None, None)
                };
                Ok(self.wrap_not_null(ValueType::Decimal {
                    keyword: Keyword::new(kw),
                    precision,
                    scale,
                }))
            }

            // ── Float types ────────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "FLOAT16") || kw_eq(s, "HALF") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::Float16 {
                    keyword: Keyword::new(kw),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "FLOAT32") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Float32 {
                    keyword: Keyword::new("FLOAT32"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "FLOAT64") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Float64 {
                    keyword: Keyword::new("FLOAT64"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "FLOAT128") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Float128))
            }
            Some(Token::Ident(s)) if kw_eq(s, "FLOAT256") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Float256))
            }
            Some(Token::Ident(s)) if kw_eq(s, "FLOAT") => {
                self.advance();
                if self.eat_token(&Token::LParen) {
                    let precision = self.expect_unsigned_int()?;
                    let scale = if self.eat_token(&Token::Comma) {
                        Some(self.expect_unsigned_int()?)
                    } else {
                        None
                    };
                    self.expect_token(&Token::RParen)?;
                    Ok(self.wrap_not_null(ValueType::FloatPrecision { precision, scale }))
                } else {
                    Ok(self.wrap_not_null(ValueType::Float32 {
                        keyword: Keyword::new("FLOAT"),
                    }))
                }
            }
            Some(Token::Ident(s)) if kw_eq(s, "REAL") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Float32 {
                    keyword: Keyword::new("REAL"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "DOUBLE") => {
                self.advance();
                // DOUBLE PRECISION?
                let has_precision = self.eat_keyword("PRECISION");
                let kw = if has_precision {
                    "DOUBLE PRECISION"
                } else {
                    "DOUBLE"
                };
                Ok(self.wrap_not_null(ValueType::Float64 {
                    keyword: Keyword::new(kw),
                }))
            }

            // ── Temporal types ─────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "DATE") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Date))
            }
            Some(Token::Ident(s)) if kw_eq(s, "ZONED") => {
                self.advance();
                if self.eat_keyword("DATETIME") {
                    Ok(self.wrap_not_null(ValueType::ZonedDateTime {
                        keyword: Keyword::new("ZONED DATETIME"),
                    }))
                } else if self.eat_keyword("TIME") {
                    Ok(self.wrap_not_null(ValueType::ZonedTime {
                        keyword: Keyword::new("ZONED TIME"),
                    }))
                } else {
                    Err(self.expected("DATETIME or TIME after ZONED"))
                }
            }
            Some(Token::Ident(s)) if kw_eq(s, "LOCAL") => {
                self.advance();
                if self.eat_keyword("DATETIME") {
                    Ok(self.wrap_not_null(ValueType::LocalDateTime {
                        keyword: Keyword::new("LOCAL DATETIME"),
                    }))
                } else if self.eat_keyword("TIME") {
                    Ok(self.wrap_not_null(ValueType::LocalTime {
                        keyword: Keyword::new("LOCAL TIME"),
                    }))
                } else if self.eat_keyword("TIMESTAMP") {
                    Ok(self.wrap_not_null(ValueType::LocalDateTime {
                        keyword: Keyword::new("LOCAL TIMESTAMP"),
                    }))
                } else {
                    Err(self.expected("DATETIME, TIME, or TIMESTAMP after LOCAL"))
                }
            }
            Some(Token::Ident(s)) if kw_eq(s, "DATETIME") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::DateTime))
            }
            Some(Token::Ident(s)) if kw_eq(s, "TIMESTAMP") => {
                self.advance();
                if self.eat_keyword("WITH") {
                    self.expect_keyword("TIME")?;
                    self.expect_keyword("ZONE")?;
                    Ok(self.wrap_not_null(ValueType::ZonedDateTime {
                        keyword: Keyword::new("TIMESTAMP WITH TIME ZONE"),
                    }))
                } else if self.eat_keyword("WITHOUT") {
                    self.expect_keyword("TIME")?;
                    self.expect_keyword("ZONE")?;
                    Ok(self.wrap_not_null(ValueType::LocalDateTime {
                        keyword: Keyword::new("TIMESTAMP WITHOUT TIME ZONE"),
                    }))
                } else {
                    // bare TIMESTAMP
                    Ok(self.wrap_not_null(ValueType::Timestamp))
                }
            }
            Some(Token::Ident(s)) if kw_eq(s, "TIME") => {
                self.advance();
                if self.eat_keyword("WITH") {
                    self.expect_keyword("TIME")?;
                    self.expect_keyword("ZONE")?;
                    Ok(self.wrap_not_null(ValueType::ZonedTime {
                        keyword: Keyword::new("TIME WITH TIME ZONE"),
                    }))
                } else if self.eat_keyword("WITHOUT") {
                    self.expect_keyword("TIME")?;
                    self.expect_keyword("ZONE")?;
                    Ok(self.wrap_not_null(ValueType::LocalTime {
                        keyword: Keyword::new("TIME WITHOUT TIME ZONE"),
                    }))
                } else {
                    // bare TIME
                    Ok(self.wrap_not_null(ValueType::Time))
                }
            }
            Some(Token::Ident(s)) if kw_eq(s, "DURATION") => {
                self.advance();
                // GQL: temporalDurationType requires qualifier in parens.
                // Bare DURATION is not a valid type.
                self.expect_token(&Token::LParen)?;
                if self.eat_keyword("YEAR") {
                    self.expect_keyword("TO")?;
                    self.expect_keyword("MONTH")?;
                    self.expect_token(&Token::RParen)?;
                    Ok(self.wrap_not_null(ValueType::DurationYearToMonth))
                } else if self.eat_keyword("DAY") {
                    self.expect_keyword("TO")?;
                    self.expect_keyword("SECOND")?;
                    self.expect_token(&Token::RParen)?;
                    Ok(self.wrap_not_null(ValueType::DurationDayToSecond))
                } else {
                    Err(self.expected("YEAR TO MONTH or DAY TO SECOND"))
                }
            }

            // ── ZONED_DATETIME / ZONED_TIME / LOCAL_DATETIME / LOCAL_TIME /
            //    LOCAL_TIMESTAMP — single-token variants ────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "ZONED_DATETIME") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::ZonedDateTime {
                    keyword: Keyword::new("ZONED_DATETIME"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "ZONED_TIME") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::ZonedTime {
                    keyword: Keyword::new("ZONED_TIME"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "LOCAL_DATETIME") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::LocalDateTime {
                    keyword: Keyword::new("LOCAL_DATETIME"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "LOCAL_TIME") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::LocalTime {
                    keyword: Keyword::new("LOCAL_TIME"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "LOCAL_TIMESTAMP") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::LocalDateTime {
                    keyword: Keyword::new("LOCAL_TIMESTAMP"),
                }))
            }

            // ── Path ───────────────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "PATH") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Path))
            }

            // ── List/Array prefix form: LIST<type> / ARRAY<type> ───────────
            Some(Token::Ident(s)) if kw_eq(s, "LIST") || kw_eq(s, "ARRAY") => {
                let list_kw = s.to_ascii_uppercase();
                self.advance();
                if self.eat_token(&Token::Lt) {
                    let element_type = self.parse_value_type()?;
                    self.expect_token(&Token::Gt)?;
                    let max_length = self.try_parse_bracket_length()?;
                    Ok(self.wrap_not_null(ValueType::List {
                        keyword: Keyword::new(list_kw),
                        element_type: Box::new(element_type),
                        max_length,
                    }))
                } else {
                    // bare LIST/ARRAY with no angle brackets — untyped list
                    let max_length = self.try_parse_bracket_length()?;
                    Ok(self.wrap_not_null(ValueType::List {
                        keyword: Keyword::new(list_kw),
                        element_type: Box::new(ValueType::AnyValue),
                        max_length,
                    }))
                }
            }

            // ── Record ─────────────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "RECORD") => {
                self.advance();
                if self.at_token(&Token::LBrace) {
                    let fields = self.parse_field_types_specification()?;
                    Ok(self.wrap_not_null(ValueType::Record {
                        record_keyword: true,
                        fields,
                    }))
                } else {
                    // bare RECORD = ANY RECORD
                    Ok(self.wrap_not_null(ValueType::Record {
                        record_keyword: true,
                        fields: vec![],
                    }))
                }
            }

            // ── Bare `{field: type, ...}` — record without RECORD keyword ─
            Some(Token::LBrace) => {
                let fields = self.parse_field_types_specification()?;
                Ok(self.wrap_not_null(ValueType::Record {
                    record_keyword: false,
                    fields,
                }))
            }

            // ── ANY ... ────────────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "ANY") => {
                self.advance();
                // ANY PROPERTY GRAPH
                if self.at_keyword("PROPERTY") {
                    let save = self.save();
                    self.advance(); // consume PROPERTY
                    if self.eat_keyword("GRAPH") {
                        return Ok(self.wrap_not_null(ValueType::GraphRef {
                            keyword: Keyword::new("ANY PROPERTY GRAPH"),
                        }));
                    }
                    // ANY PROPERTY VALUE
                    if self.eat_keyword("VALUE") {
                        return Ok(self.wrap_not_null(ValueType::AnyPropertyValue));
                    }
                    // Backtrack — PROPERTY didn't lead anywhere valid
                    self.restore(save);
                    self.advance(); // re-consume PROPERTY
                    return Err(self.expected("GRAPH or VALUE after ANY PROPERTY"));
                }
                // ANY GRAPH
                if self.eat_keyword("GRAPH") {
                    return Ok(self.wrap_not_null(ValueType::GraphRef {
                        keyword: Keyword::new("ANY GRAPH"),
                    }));
                }
                // ANY NODE / ANY VERTEX
                if self.at_keyword("NODE") || self.at_keyword("VERTEX") {
                    let kw = format!("ANY {}", self.current_ident_upper());
                    self.advance();
                    return Ok(self.wrap_not_null(ValueType::NodeRef {
                        keyword: Keyword::new(kw),
                        label: None,
                    }));
                }
                // ANY EDGE / ANY RELATIONSHIP
                if self.at_keyword("EDGE") || self.at_keyword("RELATIONSHIP") {
                    let kw = format!("ANY {}", self.current_ident_upper());
                    self.advance();
                    return Ok(self.wrap_not_null(ValueType::EdgeRef {
                        keyword: Keyword::new(kw),
                        label: None,
                    }));
                }
                // ANY RECORD
                if self.eat_keyword("RECORD") {
                    return Ok(self.wrap_not_null(ValueType::Record {
                        record_keyword: true,
                        fields: vec![],
                    }));
                }
                // ANY VALUE <type | type | ...>
                if self.at_keyword("VALUE") {
                    self.advance(); // consume VALUE
                    if self.eat_token(&Token::Lt) {
                        let mut members = vec![self.parse_value_type_primary()?];
                        while self.eat_token(&Token::Pipe) {
                            members.push(self.parse_value_type_primary()?);
                        }
                        self.expect_token(&Token::Gt)?;
                        return Ok(ValueType::ClosedDynamicUnion(members));
                    }
                    return Ok(self.wrap_not_null(ValueType::AnyValue));
                }
                // ANY <type | type | ...> — closed dynamic union
                if self.eat_token(&Token::Lt) {
                    let mut members = vec![self.parse_value_type_primary()?];
                    while self.eat_token(&Token::Pipe) {
                        members.push(self.parse_value_type_primary()?);
                    }
                    self.expect_token(&Token::Gt)?;
                    return Ok(ValueType::ClosedDynamicUnion(members));
                }
                // bare ANY [NOT NULL]
                Ok(self.wrap_not_null(ValueType::Any))
            }

            // ── Reference types (without ANY prefix) ───────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "NODE") || kw_eq(s, "VERTEX") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::NodeRef {
                    keyword: Keyword::new(kw),
                    label: None,
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "EDGE") || kw_eq(s, "RELATIONSHIP") => {
                let kw = s.to_ascii_uppercase();
                self.advance();
                Ok(self.wrap_not_null(ValueType::EdgeRef {
                    keyword: Keyword::new(kw),
                    label: None,
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "GRAPH") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::GraphRef {
                    keyword: Keyword::new("GRAPH"),
                }))
            }

            // ── Immaterial types ───────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "NULL") => {
                self.advance();
                // NULL NOT NULL => NotNull(Null) = NOTHING equivalent
                if self.at_keyword("NOT") && self.at_keyword_ahead(1, "NULL") {
                    self.advance(); // NOT
                    self.advance(); // NULL
                    Ok(ValueType::NotNull(Box::new(ValueType::Null)))
                } else {
                    Ok(ValueType::Null)
                }
            }
            Some(Token::Ident(s)) if kw_eq(s, "NOTHING") => {
                self.advance();
                Ok(ValueType::Nothing)
            }

            // ── SMALL INTEGER / BIG INTEGER (verbose signed) ───────────────
            Some(Token::Ident(s)) if kw_eq(s, "SMALL") => {
                self.advance();
                self.expect_keyword("INTEGER")?;
                Ok(self.wrap_not_null(ValueType::Int16 {
                    keyword: Keyword::new("SMALL INTEGER"),
                }))
            }
            Some(Token::Ident(s)) if kw_eq(s, "BIG") => {
                self.advance();
                self.expect_keyword("INTEGER")?;
                Ok(self.wrap_not_null(ValueType::Int64 {
                    keyword: Keyword::new("BIG INTEGER"),
                }))
            }

            // ── TINYINT (sql-compat alias for INT8; not in GQL) ────────
            #[cfg(feature = "sql-compat")]
            Some(Token::Ident(s)) if kw_eq(s, "TINYINT") => {
                self.advance();
                Ok(self.wrap_not_null(ValueType::Int8 {
                    keyword: Keyword::new("TINYINT"),
                }))
            }

            // ── PROPERTY GRAPH ─────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "PROPERTY") => {
                self.advance();
                self.expect_keyword("GRAPH")?;
                Ok(self.wrap_not_null(ValueType::GraphRef {
                    keyword: Keyword::new("PROPERTY GRAPH"),
                }))
            }

            // ── BINDING TABLE ──────────────────────────────────────────────
            Some(Token::Ident(s)) if kw_eq(s, "BINDING") => {
                self.advance();
                self.expect_keyword("TABLE")?;
                let fields = if self.at_token(&Token::LBrace) {
                    Some(self.parse_field_types_specification()?)
                } else {
                    None
                };
                Ok(self.wrap_not_null(ValueType::BindingTableRef { fields }))
            }

            // ── Host extension value type ───────────────────────────────────
            // Unknown identifiers in type position are accepted and deferred
            // to host-side extension type resolution.
            Some(Token::Ident(_)) => {
                let name = self.parse_object_name()?;
                Ok(self.wrap_not_null(ValueType::ExtensionType { name }))
            }

            _ => Err(self.expected("value type")),
        }
    }

    // ── Verbose integer helpers ────────────────────────────────────────────

    /// Parses the verbose integer type after SIGNED keyword.
    /// Grammar: SIGNED verboseBinaryExactNumericType
    fn parse_verbose_signed_integer(&mut self) -> Result<ValueType, GqlError> {
        self.parse_verbose_binary_exact_numeric(false, "SIGNED")
    }

    /// Parses the verbose integer type after UNSIGNED keyword.
    /// Grammar: UNSIGNED verboseBinaryExactNumericType
    fn parse_verbose_unsigned_integer(&mut self) -> Result<ValueType, GqlError> {
        self.parse_verbose_binary_exact_numeric(true, "UNSIGNED")
    }

    /// Parses verboseBinaryExactNumericType: INTEGER8/16/32/64/128/256,
    /// SMALL INTEGER, INTEGER[(precision)], BIG INTEGER.
    fn parse_verbose_binary_exact_numeric(
        &mut self,
        unsigned: bool,
        prefix: &str,
    ) -> Result<ValueType, GqlError> {
        if self.eat_keyword("INTEGER8") {
            let kw = format!("{prefix} INTEGER8");
            let ty = if unsigned {
                ValueType::Uint8 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int8 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("INTEGER16") {
            let kw = format!("{prefix} INTEGER16");
            let ty = if unsigned {
                ValueType::Uint16 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int16 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("INTEGER32") {
            let kw = format!("{prefix} INTEGER32");
            let ty = if unsigned {
                ValueType::Uint32 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int32 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("INTEGER64") {
            let kw = format!("{prefix} INTEGER64");
            let ty = if unsigned {
                ValueType::Uint64 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int64 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("INTEGER128") {
            let kw = format!("{prefix} INTEGER128");
            let ty = if unsigned {
                ValueType::Uint128 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int128 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("INTEGER256") {
            let kw = format!("{prefix} INTEGER256");
            let ty = if unsigned {
                ValueType::Uint256 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int256 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("SMALL") {
            self.expect_keyword("INTEGER")?;
            let kw = format!("{prefix} SMALL INTEGER");
            let ty = if unsigned {
                ValueType::Uint16 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int16 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("BIG") {
            self.expect_keyword("INTEGER")?;
            let kw = format!("{prefix} BIG INTEGER");
            let ty = if unsigned {
                ValueType::Uint64 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int64 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("INTEGER") {
            let precision = self.try_parse_paren_single()?;
            let kw = format!("{prefix} INTEGER");
            let ty = match precision {
                Some(p) => {
                    if unsigned {
                        ValueType::UintPrecision {
                            keyword: Keyword::new(kw),
                            precision: p,
                        }
                    } else {
                        ValueType::IntPrecision {
                            keyword: Keyword::new(kw),
                            precision: p,
                        }
                    }
                }
                None => {
                    if unsigned {
                        ValueType::Uint32 {
                            keyword: Keyword::new(kw),
                        }
                    } else {
                        ValueType::Int32 {
                            keyword: Keyword::new(kw),
                        }
                    }
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("SMALLINT") {
            let kw = format!("{prefix} SMALLINT");
            let ty = if unsigned {
                ValueType::Uint16 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int16 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("BIGINT") {
            let kw = format!("{prefix} BIGINT");
            let ty = if unsigned {
                ValueType::Uint64 {
                    keyword: Keyword::new(kw),
                }
            } else {
                ValueType::Int64 {
                    keyword: Keyword::new(kw),
                }
            };
            Ok(self.wrap_not_null(ty))
        } else if self.eat_keyword("INT") {
            let precision = self.try_parse_paren_single()?;
            let kw = format!("{prefix} INT");
            let ty = match precision {
                Some(p) => {
                    if unsigned {
                        ValueType::UintPrecision {
                            keyword: Keyword::new(kw),
                            precision: p,
                        }
                    } else {
                        ValueType::IntPrecision {
                            keyword: Keyword::new(kw),
                            precision: p,
                        }
                    }
                }
                None => {
                    if unsigned {
                        ValueType::Uint32 {
                            keyword: Keyword::new(kw),
                        }
                    } else {
                        ValueType::Int32 {
                            keyword: Keyword::new(kw),
                        }
                    }
                }
            };
            Ok(self.wrap_not_null(ty))
        } else {
            Err(self.expected("integer type after SIGNED/UNSIGNED"))
        }
    }

    // ── Helpers ────────────────────────────────────────────────────────────

    /// Tries to parse `( unsigned_int )` returning `Some(value)` or `None`.
    fn try_parse_paren_single(&mut self) -> Result<Option<u64>, GqlError> {
        if self.eat_token(&Token::LParen) {
            let v = self.expect_unsigned_int()?;
            self.expect_token(&Token::RParen)?;
            Ok(Some(v))
        } else {
            Ok(None)
        }
    }

    /// Tries to parse `[ unsigned_int ]` returning `Some(value)` or `None`.
    fn try_parse_bracket_length(&mut self) -> Result<Option<u64>, GqlError> {
        if self.eat_token(&Token::LBracket) {
            let v = self.expect_unsigned_int()?;
            self.expect_token(&Token::RBracket)?;
            Ok(Some(v))
        } else {
            Ok(None)
        }
    }

    /// Consumes `NOT NULL` if present and returns true.
    pub(crate) fn eat_not_null(&mut self) -> bool {
        if self.at_keyword("NOT") && self.at_keyword_ahead(1, "NULL") {
            self.advance(); // NOT
            self.advance(); // NULL
            true
        } else {
            false
        }
    }

    /// If `NOT NULL` follows, wraps the type in `ValueType::NotNull`.
    fn wrap_not_null(&mut self, ty: ValueType) -> ValueType {
        if self.eat_not_null() {
            ValueType::NotNull(Box::new(ty))
        } else {
            ty
        }
    }

    /// Parses `{ field_name typed? value_type, ... }`.
    fn parse_field_types_specification(&mut self) -> Result<Vec<RecordFieldType>, GqlError> {
        self.expect_token(&Token::LBrace)?;
        let mut fields = Vec::new();
        if !self.at_token(&Token::RBrace) {
            fields.push(self.parse_field_type()?);
            while self.eat_token(&Token::Comma) {
                fields.push(self.parse_field_type()?);
            }
        }
        self.expect_token(&Token::RBrace)?;
        Ok(fields)
    }

    /// Parses a single record field: `field_name typed? value_type`.
    fn parse_field_type(&mut self) -> Result<RecordFieldType, GqlError> {
        let start = self.save();
        let name = self.expect_ident()?;
        // Optional TYPED / :: prefix before the type
        let typed_prefix = if self.eat_token(&Token::DoubleColon) {
            TypedPrefix::DoubleColon
        } else if self.eat_keyword("TYPED") {
            TypedPrefix::Typed
        } else {
            TypedPrefix::None
        };
        let value_type = self.parse_value_type()?;
        Ok(RecordFieldType {
            span: self.span_since(start),
            name,
            typed_prefix,
            value_type,
        })
    }

    /// Returns the current token's identifier text in uppercase.
    /// Panics if the current token is not an identifier.
    pub(super) fn current_ident_upper(&self) -> String {
        match self.peek() {
            Some(Token::Ident(s)) => s.to_ascii_uppercase(),
            _ => String::new(),
        }
    }
}

/// Case-insensitive keyword comparison helper.
fn kw_eq(s: &str, kw: &str) -> bool {
    s.eq_ignore_ascii_case(kw)
}
