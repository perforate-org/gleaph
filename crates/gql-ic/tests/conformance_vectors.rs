use gleaph_gql::Value;
use gleaph_gql_ic::{IcExtensionBinaryDecode, Principal, PrincipalValue};
use serde_json::Value as JsonValue;

#[derive(serde::Deserialize)]
struct Fixture {
    version: u32,
    vectors: Vec<Vector>,
    invalid_binary_vectors: Vec<InvalidBinaryVector>,
}

#[derive(serde::Deserialize)]
struct Vector {
    name: String,
    value: JsonValue,
    canonical_bytes_hex: String,
}

#[derive(serde::Deserialize)]
struct InvalidBinaryVector {
    name: String,
    bytes_hex: String,
}

fn hex_bytes(value: &str) -> Vec<u8> {
    assert!(value.len().is_multiple_of(2), "odd-length hex: {value}");
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("hex is UTF-8");
            u8::from_str_radix(text, 16).expect("valid hex")
        })
        .collect()
}

fn value_from_fixture(value: &JsonValue) -> Value {
    let object = value.as_object().expect("value object");
    assert_eq!(object.len(), 1, "fixture value must have one tag");
    let (tag, payload) = object.iter().next().expect("fixture tag");
    match tag.as_str() {
        "Null" => Value::Null,
        "Bool" => Value::Bool(payload.as_bool().expect("Bool payload")),
        "Int64" => Value::Int64(
            payload
                .as_str()
                .expect("Int64 string")
                .parse()
                .expect("Int64"),
        ),
        "Uint64" => Value::Uint64(
            payload
                .as_str()
                .expect("Uint64 string")
                .parse()
                .expect("Uint64"),
        ),
        "Float64" => Value::Float64(payload.as_f64().expect("Float64 payload")),
        "Text" => Value::Text(payload.as_str().expect("Text payload").to_owned()),
        "Bytes" => Value::Bytes(hex_bytes(payload.as_str().expect("Bytes hex"))),
        "List" => Value::List(
            payload
                .as_array()
                .expect("List payload")
                .iter()
                .map(value_from_fixture)
                .collect(),
        ),
        "Record" => Value::Record(
            payload
                .as_object()
                .expect("Record payload")
                .iter()
                .map(|(key, value)| (key.clone(), value_from_fixture(value)))
                .collect(),
        ),
        "DateTime" => {
            let payload = payload.as_object().expect("DateTime payload");
            Value::DateTime(
                payload["seconds"]
                    .as_str()
                    .expect("seconds string")
                    .parse()
                    .expect("seconds"),
                payload["nanos"].as_u64().expect("nanos") as u32,
            )
        }
        "Principal" => Value::from(PrincipalValue(
            Principal::from_text(payload.as_str().expect("Principal text")).expect("Principal"),
        )),
        other => panic!("unsupported fixture tag: {other}"),
    }
}

#[test]
fn rust_encoder_matches_shared_sdk_vectors() {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "../../graph-kernel/conformance/gql_value_vectors.json"
    ))
    .expect("decode conformance fixture");
    assert_eq!(fixture.version, 1);
    assert!(!fixture.vectors.is_empty());

    for vector in fixture.vectors {
        let actual = value_from_fixture(&vector.value)
            .to_binary_bytes()
            .unwrap_or_else(|error| panic!("{}: encode failed: {error}", vector.name));
        assert_eq!(
            actual,
            hex_bytes(&vector.canonical_bytes_hex),
            "{}",
            vector.name
        );
    }

    for vector in fixture.invalid_binary_vectors {
        let error = Value::from_binary_bytes_with_extensions(
            &hex_bytes(&vector.bytes_hex),
            &IcExtensionBinaryDecode::INSTANCE,
        )
        .expect_err("invalid binary vector must be rejected");
        assert!(!error.to_string().is_empty(), "{}", vector.name);
    }
}
