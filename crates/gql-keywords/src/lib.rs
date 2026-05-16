//! GQL **reserved** and **prereserved** keywords for compile-time and parser checks.
//!
//! Reserved words follow ISO/IEC 39075 §21.3. Prereserved words are the additional
//! spellings the standard classifies as prereserved (they may become reserved in a
//! future edition).

/// GQL reserved keywords (ISO/IEC 39075 §21.3).
///
/// This is the full set from GQL. Non-reserved words (like GRAPH, EDGE, etc.)
/// are NOT included — they can be used as identifiers without quoting.
///
/// Matching is case-insensitive (compares against the uppercase form of `s`).
#[must_use]
pub fn is_reserved_keyword(s: &str) -> bool {
    matches!(
        s.to_ascii_uppercase().as_str(),
        "ABS"
            | "ACOS"
            | "ALL"
            | "ALL_DIFFERENT"
            | "AND"
            | "ANY"
            | "ARRAY"
            | "AS"
            | "ASC"
            | "ASCENDING"
            | "ASIN"
            | "AT"
            | "ATAN"
            | "AVG"
            | "BIG"
            | "BIGINT"
            | "BINARY"
            | "BOOL"
            | "BOOLEAN"
            | "BOTH"
            | "BTRIM"
            | "BY"
            | "BYTE_LENGTH"
            | "BYTES"
            | "CALL"
            | "CARDINALITY"
            | "CASE"
            | "CAST"
            | "CEIL"
            | "CEILING"
            | "CHAR"
            | "CHAR_LENGTH"
            | "CHARACTER_LENGTH"
            | "CHARACTERISTICS"
            | "CLOSE"
            | "COALESCE"
            | "COLLECT_LIST"
            | "COMMIT"
            | "COPY"
            | "COS"
            | "COSH"
            | "COT"
            | "COUNT"
            | "CREATE"
            | "CURRENT_DATE"
            | "CURRENT_GRAPH"
            | "CURRENT_PROPERTY_GRAPH"
            | "CURRENT_SCHEMA"
            | "CURRENT_TIME"
            | "CURRENT_TIMESTAMP"
            | "DATE"
            | "DATETIME"
            | "DAY"
            | "DEC"
            | "DECIMAL"
            | "DEGREES"
            | "DELETE"
            | "DESC"
            | "DESCENDING"
            | "DETACH"
            | "DISTINCT"
            | "DOUBLE"
            | "DROP"
            | "DURATION"
            | "DURATION_BETWEEN"
            | "ELEMENT_ID"
            | "ELSE"
            | "END"
            | "EXCEPT"
            | "EXISTS"
            | "EXP"
            | "FILTER"
            | "FINISH"
            | "FLOAT"
            | "FLOAT16"
            | "FLOAT32"
            | "FLOAT64"
            | "FLOAT128"
            | "FLOAT256"
            | "FLOOR"
            | "FOR"
            | "FROM"
            | "GROUP"
            | "HAVING"
            | "HOME_GRAPH"
            | "HOME_PROPERTY_GRAPH"
            | "HOME_SCHEMA"
            | "HOUR"
            | "IF"
            | "IN"
            | "INSERT"
            | "INT"
            | "INT8"
            | "INT16"
            | "INT32"
            | "INT64"
            | "INT128"
            | "INT256"
            | "INTEGER"
            | "INTEGER8"
            | "INTEGER16"
            | "INTEGER32"
            | "INTEGER64"
            | "INTEGER128"
            | "INTEGER256"
            | "INTERSECT"
            | "INTERVAL"
            | "IS"
            | "LEADING"
            | "LEFT"
            | "LET"
            | "LIKE"
            | "LIMIT"
            | "LIST"
            | "LN"
            | "LOCAL"
            | "LOCAL_DATETIME"
            | "LOCAL_TIME"
            | "LOCAL_TIMESTAMP"
            | "LOG"
            | "LOG10"
            | "LOWER"
            | "LTRIM"
            | "MATCH"
            | "MAX"
            | "MIN"
            | "MINUTE"
            | "MOD"
            | "MONTH"
            | "NEXT"
            | "NODETACH"
            | "NORMALIZE"
            | "NOT"
            | "NOTHING"
            | "NULL"
            | "NULLS"
            | "NULLIF"
            | "OCTET_LENGTH"
            | "OF"
            | "OFFSET"
            | "OPTIONAL"
            | "OR"
            | "ORDER"
            | "OTHERWISE"
            | "PARAMETER"
            | "PARAMETERS"
            | "PATH"
            | "PATH_LENGTH"
            | "PATHS"
            | "PERCENTILE_CONT"
            | "PERCENTILE_DISC"
            | "POWER"
            | "PRECISION"
            | "PROPERTY_EXISTS"
            | "RADIANS"
            | "REAL"
            | "RECORD"
            | "REMOVE"
            | "REPLACE"
            | "RESET"
            | "RETURN"
            | "RIGHT"
            | "ROLLBACK"
            | "RTRIM"
            | "SAME"
            | "SCHEMA"
            | "SECOND"
            | "SELECT"
            | "SESSION"
            | "SESSION_USER"
            | "SET"
            | "SIGNED"
            | "SIN"
            | "SINH"
            | "SIZE"
            | "SKIP"
            | "SMALL"
            | "SMALLINT"
            | "SQRT"
            | "START"
            | "STDDEV_POP"
            | "STDDEV_SAMP"
            | "STRING"
            | "SUM"
            | "TAN"
            | "TANH"
            | "THEN"
            | "TIME"
            | "TIMESTAMP"
            | "TRAILING"
            | "TRIM"
            | "TYPED"
            | "UBIGINT"
            | "UINT"
            | "UINT8"
            | "UINT16"
            | "UINT32"
            | "UINT64"
            | "UINT128"
            | "UINT256"
            | "UNION"
            | "UNSIGNED"
            | "UPPER"
            | "USE"
            | "USMALLINT"
            | "VALUE"
            | "VARBINARY"
            | "VARCHAR"
            | "VARIABLE"
            | "WHEN"
            | "WHERE"
            | "WITH"
            | "XOR"
            | "YEAR"
            | "YIELD"
            | "ZONED"
            | "ZONED_DATETIME"
            | "ZONED_TIME"
    )
}

/// GQL *prereserved* keywords (ISO/IEC 39075; may become reserved in a future edition).
///
/// Matching is case-insensitive (compares against the uppercase form of `s`).
#[must_use]
pub fn is_prereserved_keyword(s: &str) -> bool {
    matches!(
        s.to_ascii_uppercase().as_str(),
        "ABSTRACT"
            | "AGGREGATE"
            | "AGGREGATES"
            | "ALTER"
            | "CATALOG"
            | "CLEAR"
            | "CLONE"
            | "CONSTRAINT"
            | "CURRENT_ROLE"
            | "CURRENT_USER"
            | "DATA"
            | "DIRECTORY"
            | "DRYRUN"
            | "EXACT"
            | "EXISTING"
            | "FUNCTION"
            | "GQLSTATUS"
            | "GRANT"
            | "INSTANT"
            | "INFINITY"
            | "NUMBER"
            | "NUMERIC"
            | "ON"
            | "OPEN"
            | "PARTITION"
            | "PROCEDURE"
            | "PRODUCT"
            | "PROJECT"
            | "QUERY"
            | "RECORDS"
            | "REFERENCE"
            | "RENAME"
            | "REVOKE"
            | "SUBSTRING"
            | "SYSTEM_USER"
            | "TEMPORAL"
            | "UNIQUE"
            | "UNIT"
            | "VALUES"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_insensitive_reserved() {
        assert!(is_reserved_keyword("MATCH"));
        assert!(is_reserved_keyword("match"));
        assert!(is_reserved_keyword("Select"));
    }

    #[test]
    fn graph_is_not_reserved() {
        assert!(!is_reserved_keyword("GRAPH"));
        assert!(!is_reserved_keyword("graph"));
    }

    #[test]
    fn extension_like_prefix_ok() {
        assert!(!is_reserved_keyword("IC"));
        assert!(!is_reserved_keyword("GLEAPH"));
    }

    #[test]
    fn prereserved_case_insensitive() {
        assert!(is_prereserved_keyword("ON"));
        assert!(is_prereserved_keyword("on"));
        assert!(is_prereserved_keyword("Function"));
    }

    #[test]
    fn prereserved_disjoint_from_reserved_examples() {
        assert!(is_prereserved_keyword("VALUES"));
        assert!(!is_reserved_keyword("VALUES"));
        assert!(is_reserved_keyword("VALUE"));
        assert!(!is_prereserved_keyword("VALUE"));
    }
}
