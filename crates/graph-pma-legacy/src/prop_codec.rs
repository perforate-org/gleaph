use gleaph_graph_kernel::PropertyMap;

use crate::layout::{LayoutError, LayoutResult};

pub fn encode_property_map(properties: &PropertyMap, out: &mut Vec<u8>) {
    write_u32(out, properties.len() as u32);
    for (key, value) in properties {
        write_string(out, key);
        encode_value(value, out);
    }
}

pub fn decode_property_map(input: &[u8], cursor: &mut usize) -> LayoutResult<PropertyMap> {
    let len = read_u32(input, cursor)? as usize;
    let mut map = PropertyMap::new();
    for _ in 0..len {
        let key = read_string(input, cursor)?;
        let value = decode_value(input, cursor)?;
        map.insert(key, value);
    }
    Ok(map)
}

pub fn encode_value_bytes(value: &gleaph_gql::Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value(value, &mut out);
    out
}

pub fn decode_value_bytes(bytes: &[u8]) -> Option<gleaph_gql::Value> {
    let mut cursor = 0;
    decode_value(bytes, &mut cursor).ok()
}

pub fn encode_u64_list(ids: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + ids.len() * 8);
    out.extend_from_slice(&(ids.len() as u32).to_be_bytes());
    for id in ids {
        out.extend_from_slice(&id.to_be_bytes());
    }
    out
}

pub fn decode_u64_list(bytes: &[u8]) -> Vec<u64> {
    if bytes.len() < 4 {
        return Vec::new();
    }
    let len = u32::from_be_bytes(bytes[0..4].try_into().expect("slice length checked")) as usize;
    let mut ids = Vec::with_capacity(len);
    let mut cursor = 4;
    for _ in 0..len {
        if cursor + 8 > bytes.len() {
            break;
        }
        ids.push(u64::from_be_bytes(
            bytes[cursor..cursor + 8]
                .try_into()
                .expect("slice length checked"),
        ));
        cursor += 8;
    }
    ids
}

pub fn write_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

pub fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn write_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

pub fn write_string(out: &mut Vec<u8>, value: &str) {
    write_bytes(out, value.as_bytes());
}

pub fn read_u8(input: &[u8], cursor: &mut usize) -> LayoutResult<u8> {
    if *cursor >= input.len() {
        return Err(LayoutError::UnexpectedEof);
    }
    let value = input[*cursor];
    *cursor += 1;
    Ok(value)
}

pub fn read_u32(input: &[u8], cursor: &mut usize) -> LayoutResult<u32> {
    let bytes = read_fixed::<4>(input, cursor)?;
    Ok(u32::from_le_bytes(bytes))
}

pub fn read_u64(input: &[u8], cursor: &mut usize) -> LayoutResult<u64> {
    let bytes = read_fixed::<8>(input, cursor)?;
    Ok(u64::from_le_bytes(bytes))
}

pub fn read_i64(input: &[u8], cursor: &mut usize) -> LayoutResult<i64> {
    let bytes = read_fixed::<8>(input, cursor)?;
    Ok(i64::from_le_bytes(bytes))
}

pub fn read_bytes(input: &[u8], cursor: &mut usize) -> LayoutResult<Vec<u8>> {
    let len = read_u32(input, cursor)? as usize;
    if *cursor + len > input.len() {
        return Err(LayoutError::UnexpectedEof);
    }
    let bytes = input[*cursor..*cursor + len].to_vec();
    *cursor += len;
    Ok(bytes)
}

pub fn read_string(input: &[u8], cursor: &mut usize) -> LayoutResult<String> {
    let bytes = read_bytes(input, cursor)?;
    String::from_utf8(bytes).map_err(|_| LayoutError::InvalidPayload)
}

fn read_fixed<const N: usize>(input: &[u8], cursor: &mut usize) -> LayoutResult<[u8; N]> {
    if *cursor + N > input.len() {
        return Err(LayoutError::UnexpectedEof);
    }
    let mut bytes = [0u8; N];
    bytes.copy_from_slice(&input[*cursor..*cursor + N]);
    *cursor += N;
    Ok(bytes)
}

pub fn encode_value(value: &gleaph_gql::Value, out: &mut Vec<u8>) {
    use gleaph_gql::Value;

    match value {
        Value::Null => write_u8(out, 0),
        Value::Bool(v) => {
            write_u8(out, 1);
            write_u8(out, u8::from(*v));
        }
        Value::Int64(v) => {
            write_u8(out, 2);
            write_i64(out, *v);
        }
        Value::Uint64(v) => {
            write_u8(out, 3);
            write_u64(out, *v);
        }
        Value::Text(v) => {
            write_u8(out, 4);
            write_string(out, v);
        }
        Value::Bytes(v) => {
            write_u8(out, 5);
            write_bytes(out, v);
        }
        Value::Int32(v) => {
            write_u8(out, 6);
            write_u32(out, *v as u32);
        }
        Value::Uint32(v) => {
            write_u8(out, 7);
            write_u32(out, *v);
        }
        Value::Int8(v) => {
            write_u8(out, 8);
            write_u8(out, *v as u8);
        }
        Value::Uint8(v) => {
            write_u8(out, 9);
            write_u8(out, *v);
        }
        Value::Int16(v) => {
            write_u8(out, 10);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Uint16(v) => {
            write_u8(out, 11);
            out.extend_from_slice(&v.to_le_bytes());
        }
        _ => panic!("unsupported graph-pma value persistence variant"),
    }
}

pub fn decode_value(input: &[u8], cursor: &mut usize) -> LayoutResult<gleaph_gql::Value> {
    use gleaph_gql::Value;

    match read_u8(input, cursor)? {
        0 => Ok(Value::Null),
        1 => Ok(Value::Bool(read_u8(input, cursor)? != 0)),
        2 => Ok(Value::Int64(read_i64(input, cursor)?)),
        3 => Ok(Value::Uint64(read_u64(input, cursor)?)),
        4 => Ok(Value::Text(read_string(input, cursor)?)),
        5 => Ok(Value::Bytes(read_bytes(input, cursor)?)),
        6 => Ok(Value::Int32(read_u32(input, cursor)? as i32)),
        7 => Ok(Value::Uint32(read_u32(input, cursor)?)),
        8 => Ok(Value::Int8(read_u8(input, cursor)? as i8)),
        9 => Ok(Value::Uint8(read_u8(input, cursor)?)),
        10 => {
            let bytes = read_fixed::<2>(input, cursor)?;
            Ok(Value::Int16(i16::from_le_bytes(bytes)))
        }
        11 => {
            let bytes = read_fixed::<2>(input, cursor)?;
            Ok(Value::Uint16(u16::from_le_bytes(bytes)))
        }
        _ => Err(LayoutError::InvalidPayload),
    }
}
