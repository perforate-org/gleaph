use gleaph_gql::{ClauseBreakPolicy, FormatOptions, ItemBreakPolicy, KeywordCase, format_query};
use gleaph_message_sizing::{FitError, SizeHint, SizingPolicy, adaptive_fitting_prefix};
use js_sys::{Object, Reflect};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn format_gql_query(query: &str, options: JsValue) -> Result<String, JsValue> {
    let options = options_from_js(options)?;
    format_query(query, &options).map_err(format_error)
}

/// Find the largest prefix that fits the shared inter-canister encoded-message policy.
///
/// The caller supplies the actual Candid encoder as a JavaScript callback. The sizing algorithm,
/// current platform ceiling, and fixed headroom remain owned by `gleaph-message-sizing`, so the
/// Node seeding path cannot drift from the canister-side policy.
#[wasm_bindgen]
pub fn adaptive_inter_canister_prefix(
    length: usize,
    hint: Option<usize>,
    measure: &js_sys::Function,
) -> Result<JsValue, JsValue> {
    let result = adaptive_fitting_prefix(
        length,
        hint.map(SizeHint::new),
        SizingPolicy::inter_canister(),
        |count| {
            let value = measure
                .call1(&JsValue::UNDEFINED, &JsValue::from_f64(count as f64))
                .map_err(|_| adapter_error("measure", "message-size callback failed"))?;
            let bytes = value.as_f64().ok_or_else(|| {
                adapter_error("measure", "message-size callback must return a number")
            })?;
            if !bytes.is_finite() || bytes < 0.0 || bytes > usize::MAX as f64 {
                return Err(adapter_error(
                    "measure",
                    "message-size callback returned an invalid byte length",
                ));
            }
            Ok(bytes as usize)
        },
    );

    match result {
        Ok(Some(fitted)) => {
            let value = Object::new();
            Reflect::set(
                &value,
                &JsValue::from_str("count"),
                &JsValue::from_f64(fitted.entry_count as f64),
            )
            .expect("plain JS object accepts count");
            Reflect::set(
                &value,
                &JsValue::from_str("encodedBytes"),
                &JsValue::from_f64(fitted.encoded_bytes as f64),
            )
            .expect("plain JS object accepts encodedBytes");
            Ok(value.into())
        }
        Ok(None) => Ok(JsValue::NULL),
        Err(FitError::Measure(error)) => Err(error),
        Err(FitError::NoEntryFits {
            encoded_bytes,
            hard_limit_bytes,
        }) => Err(adapter_error(
            "no-entry-fits",
            &format!(
                "single entry payload is {encoded_bytes} bytes; hard limit is {hard_limit_bytes}"
            ),
        )),
    }
}

fn options_from_js(value: JsValue) -> Result<FormatOptions, JsValue> {
    if value.is_null() || value.is_undefined() {
        return Ok(default_options());
    }
    if !value.is_object() {
        return Err(adapter_error(
            "invalid-options",
            "formatter options must be an object",
        ));
    }

    let mut options = FormatOptions::default();
    if let Some(value) = property_string(&value, "indentation")? {
        options.indentation = value;
    }
    if let Some(value) = property_number(&value, "lineWidth")? {
        if value < 1.0 || value > usize::MAX as f64 || value.fract() != 0.0 {
            return Err(adapter_error(
                "invalid-options",
                "lineWidth must be a positive integer",
            ));
        }
        options.line_width = value as usize;
    }
    if let Some(value) = property_string(&value, "keywordCase")? {
        options.keyword_case = match value.as_str() {
            "upper" => KeywordCase::Upper,
            "lower" => KeywordCase::Lower,
            _ => {
                return Err(adapter_error(
                    "invalid-options",
                    "keywordCase must be 'upper' or 'lower'",
                ));
            }
        };
    }
    if let Some(value) = property_string(&value, "clauseBreaks")? {
        options.clause_breaks = match value.as_str() {
            "every-clause" => ClauseBreakPolicy::EveryClause,
            "compact" => ClauseBreakPolicy::Compact,
            _ => {
                return Err(adapter_error(
                    "invalid-options",
                    "clauseBreaks must be 'every-clause' or 'compact'",
                ));
            }
        };
    }
    if let Some(value) = property_bool(&value, "commaAfterBreak")? {
        options.comma_after_break = value;
    }
    if let Some(value) = property_string(&value, "resultItemBreaks")? {
        options.result_item_breaks = match value.as_str() {
            "every-item" => ItemBreakPolicy::EveryItem,
            "compact" => ItemBreakPolicy::Compact,
            _ => {
                return Err(adapter_error(
                    "invalid-options",
                    "resultItemBreaks must be 'every-item' or 'compact'",
                ));
            }
        };
    }
    Ok(options)
}

fn default_options() -> FormatOptions {
    FormatOptions::default()
}

fn property(value: &JsValue, name: &str) -> Result<JsValue, JsValue> {
    Reflect::get(value, &JsValue::from_str(name)).map_err(|_| {
        adapter_error(
            "invalid-options",
            &format!("could not read formatter option '{name}'"),
        )
    })
}

fn property_string(value: &JsValue, name: &str) -> Result<Option<String>, JsValue> {
    let value = property(value, name)?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value.as_string().map(Some).ok_or_else(|| {
        adapter_error(
            "invalid-options",
            &format!("formatter option '{name}' must be a string"),
        )
    })
}

fn property_number(value: &JsValue, name: &str) -> Result<Option<f64>, JsValue> {
    let value = property(value, name)?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value.as_f64().map(Some).ok_or_else(|| {
        adapter_error(
            "invalid-options",
            &format!("formatter option '{name}' must be a number"),
        )
    })
}

fn property_bool(value: &JsValue, name: &str) -> Result<Option<bool>, JsValue> {
    let value = property(value, name)?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value.as_bool().map(Some).ok_or_else(|| {
        adapter_error(
            "invalid-options",
            &format!("formatter option '{name}' must be a boolean"),
        )
    })
}

fn format_error(error: gleaph_gql::FormatError) -> JsValue {
    match error {
        gleaph_gql::FormatError::Parse(message) => adapter_error("parse", &message),
        gleaph_gql::FormatError::Unsupported(message) => adapter_error("unsupported", &message),
        gleaph_gql::FormatError::InvalidOptions(message) => {
            adapter_error("invalid-options", &message)
        }
    }
}

fn adapter_error(kind: &str, message: &str) -> JsValue {
    let error = Object::new();
    Reflect::set(&error, &JsValue::from_str("kind"), &JsValue::from_str(kind))
        .expect("plain JS object accepts kind");
    Reflect::set(
        &error,
        &JsValue::from_str("message"),
        &JsValue::from_str(message),
    )
    .expect("plain JS object accepts message");
    error.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_social_demo_options() {
        let options = default_options();
        assert_eq!(options.indentation, "  ");
        assert_eq!(options.keyword_case, KeywordCase::Upper);
        assert_eq!(options.clause_breaks, ClauseBreakPolicy::EveryClause);
    }
}
