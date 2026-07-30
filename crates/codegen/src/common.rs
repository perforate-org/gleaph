use crate::SemanticType;

pub(crate) fn encode_expression(access: &str, semantic_type: &SemanticType) -> String {
    match semantic_type {
        SemanticType::Null => "{ Null: null }".to_string(),
        SemanticType::Bool => format!("{{ Bool: {access} }}"),
        SemanticType::Int64 => format!("{{ Int64: {access} }}"),
        SemanticType::Uint64 => format!("{{ Uint64: {access} }}"),
        SemanticType::Int128 => format!("{{ Int128: {access} }}"),
        SemanticType::Uint128 => format!("{{ Uint128: {access} }}"),
        SemanticType::Int256 => format!("{{ Int256: {access} }}"),
        SemanticType::Uint256 => format!("{{ Uint256: {access} }}"),
        SemanticType::Float64 => format!("{{ Float64: {access} }}"),
        SemanticType::Decimal => format!("{{ Decimal: {access} }}"),
        SemanticType::Text => format!("{{ Text: {access} }}"),
        SemanticType::Bytes => format!("{{ Bytes: {access} }}"),
        SemanticType::Date => format!("{{ Date: {access} }}"),
        SemanticType::Time => format!("{{ Time: {access} }}"),
        SemanticType::Principal => format!("{{ Principal: {access} }}"),
        SemanticType::List { element } => format!(
            "{{ List: {access}.map((value) => {}) }}",
            encode_expression("value", element)
        ),
    }
}

pub(crate) fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

pub(crate) fn pascal_case(name: &str) -> String {
    let mut result = String::new();
    for part in name.split(|c: char| !c.is_ascii_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            result.push(first.to_ascii_uppercase());
            result.extend(chars);
        }
    }
    if result.is_empty() {
        "PreparedOperation".to_string()
    } else if result.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("Operation{result}")
    } else {
        result
    }
}

pub(crate) fn ts_property(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !name.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        name.to_string()
    } else {
        format!("{name:?}")
    }
}
