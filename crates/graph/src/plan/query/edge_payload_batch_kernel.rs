//! Width-dispatched edge-payload batch kernels.

use std::cmp::Ordering;

use gleaph_gql::ast::CmpOp;
use gleaph_graph_kernel::entry::{
    EdgePayloadEncoding, EdgePayloadProfile, PreparedEdgePayloadDecoder, decode_edge_weight,
};
use half::f16;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedEdgePayloadBatchKernel {
    byte_width: u16,
    encoding: EdgePayloadEncoding,
}

impl PreparedEdgePayloadBatchKernel {
    pub(crate) fn new(byte_width: u16, encoding: EdgePayloadEncoding) -> Self {
        Self {
            byte_width,
            encoding,
        }
    }

    pub(crate) fn byte_width(&self) -> u16 {
        self.byte_width
    }

    pub(crate) fn encoding(&self) -> &EdgePayloadEncoding {
        &self.encoding
    }

    fn width_usize(&self) -> usize {
        usize::from(self.byte_width)
    }

    pub(crate) fn collect_equal_value_indices(
        &self,
        payload_bytes: &[u8],
        needle: &[u8],
        out: &mut Vec<usize>,
    ) {
        let width = self.width_usize();
        if width == 0 || needle.len() != width || !payload_bytes.len().is_multiple_of(width) {
            return;
        }
        match width {
            1 => collect_equal_w1(payload_bytes, needle[0], out),
            2 => collect_equal_w2(payload_bytes, needle, out),
            4 => collect_equal_w4(payload_bytes, needle, out),
            8 => collect_equal_w8(payload_bytes, needle, out),
            16 => collect_equal_w16(payload_bytes, needle, out),
            _ => collect_equal_fixed_width(payload_bytes, needle, width, out),
        }
    }

    pub(crate) fn collect_matching_value_indices(
        &self,
        payload_bytes: &[u8],
        op: CmpOp,
        needle: &[u8],
        out: &mut Vec<usize>,
    ) {
        if op == CmpOp::Eq {
            self.collect_equal_value_indices(payload_bytes, needle, out);
            return;
        }
        let width = self.width_usize();
        if width == 0 || needle.len() != width || !payload_bytes.len().is_multiple_of(width) {
            return;
        }
        if self.is_weight_encoding() {
            collect_cmp_weight(
                payload_bytes,
                op,
                needle,
                self.byte_width,
                &self.encoding,
                out,
            );
            return;
        }
        if matches!(self.encoding, EdgePayloadEncoding::F16) && width == 2 {
            collect_cmp_f16(payload_bytes, op, needle, out);
            return;
        }
        if self.is_unsigned_integer_encoding() {
            match width {
                1 => collect_cmp_u8(payload_bytes, op, needle[0], out),
                2 => collect_cmp_u16(payload_bytes, op, needle, out),
                4 => collect_cmp_u32(payload_bytes, op, needle, out),
                8 => collect_cmp_unsigned_scalar(payload_bytes, op, needle, 8, out),
                16 => collect_cmp_u128(payload_bytes, op, needle, out),
                _ => collect_cmp_fixed_width_bytes(payload_bytes, op, needle, width, out),
            }
            return;
        }
        if self.is_signed_integer_encoding() {
            match width {
                1 => collect_cmp_i8(payload_bytes, op, needle[0] as i8, out),
                2 => collect_cmp_i16(payload_bytes, op, needle, out),
                4 => collect_cmp_i32(payload_bytes, op, needle, out),
                8 => collect_cmp_i64(payload_bytes, op, needle, out),
                16 => collect_cmp_i128(payload_bytes, op, needle, out),
                _ => collect_cmp_fixed_width_bytes(payload_bytes, op, needle, width, out),
            }
            return;
        }
        if matches!(self.encoding, EdgePayloadEncoding::F32) && width == 4 {
            collect_cmp_f32(payload_bytes, op, needle, out);
            return;
        }
        if matches!(self.encoding, EdgePayloadEncoding::F64) && width == 8 {
            collect_cmp_f64(payload_bytes, op, needle, out);
            return;
        }
        collect_cmp_fixed_width_bytes(payload_bytes, op, needle, width, out);
    }

    fn is_unsigned_integer_encoding(&self) -> bool {
        matches!(
            self.encoding,
            EdgePayloadEncoding::RawU8
                | EdgePayloadEncoding::RawU16
                | EdgePayloadEncoding::RawU32
                | EdgePayloadEncoding::RawU64
                | EdgePayloadEncoding::RawU128
                | EdgePayloadEncoding::WeightRawU16
        )
    }

    fn is_signed_integer_encoding(&self) -> bool {
        matches!(
            self.encoding,
            EdgePayloadEncoding::RawI8
                | EdgePayloadEncoding::RawI16
                | EdgePayloadEncoding::RawI32
                | EdgePayloadEncoding::RawI64
                | EdgePayloadEncoding::RawI128
        )
    }

    fn is_weight_encoding(&self) -> bool {
        matches!(
            self.encoding,
            EdgePayloadEncoding::WeightRawU16
                | EdgePayloadEncoding::WeightLinearU16 { .. }
                | EdgePayloadEncoding::WeightLogU16 { .. }
                | EdgePayloadEncoding::WeightBinary16
        )
    }
}

fn collect_equal_fixed_width(
    payload_bytes: &[u8],
    needle: &[u8],
    width: usize,
    out: &mut Vec<usize>,
) {
    for (idx, chunk) in payload_bytes.chunks_exact(width).enumerate() {
        if chunk == needle {
            out.push(idx);
        }
    }
}

fn collect_equal_w1(payload_bytes: &[u8], needle: u8, out: &mut Vec<usize>) {
    for (idx, &byte) in payload_bytes.iter().enumerate() {
        if byte == needle {
            out.push(idx);
        }
    }
}

fn collect_equal_w2(payload_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    let needle = u16::from_le_bytes(needle.try_into().expect("w2 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(2).enumerate() {
        if u16::from_le_bytes(chunk.try_into().unwrap()) == needle {
            out.push(idx);
        }
    }
}

fn collect_equal_w4(payload_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    let needle = u32::from_le_bytes(needle.try_into().expect("w4 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(4).enumerate() {
        if u32::from_le_bytes(chunk.try_into().unwrap()) == needle {
            out.push(idx);
        }
    }
}

fn collect_equal_w8(payload_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    let needle = u64::from_le_bytes(needle.try_into().expect("w8 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(8).enumerate() {
        if u64::from_le_bytes(chunk.try_into().unwrap()) == needle {
            out.push(idx);
        }
    }
}

fn collect_equal_w16(payload_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    let needle = u128::from_le_bytes(needle.try_into().expect("w16 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(16).enumerate() {
        if u128::from_le_bytes(chunk.try_into().unwrap()) == needle {
            out.push(idx);
        }
    }
}

fn collect_cmp_u8(payload_bytes: &[u8], op: CmpOp, needle: u8, out: &mut Vec<usize>) {
    for (idx, &byte) in payload_bytes.iter().enumerate() {
        if cmp_u8(op, byte, needle) {
            out.push(idx);
        }
    }
}

fn collect_cmp_u16(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = u16::from_le_bytes(needle.try_into().expect("w2 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(2).enumerate() {
        let value = u16::from_le_bytes(chunk.try_into().unwrap());
        if cmp_u16(op, value, needle) {
            out.push(idx);
        }
    }
}

fn collect_cmp_u32(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = u32::from_le_bytes(needle.try_into().expect("w4 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(4).enumerate() {
        let value = u32::from_le_bytes(chunk.try_into().unwrap());
        if cmp_u32(op, value, needle) {
            out.push(idx);
        }
    }
}

fn collect_cmp_i8(payload_bytes: &[u8], op: CmpOp, needle: i8, out: &mut Vec<usize>) {
    for (idx, &byte) in payload_bytes.iter().enumerate() {
        if cmp_i64(op, i64::from(byte as i8), i64::from(needle)) {
            out.push(idx);
        }
    }
}

fn collect_cmp_i16(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = i16::from_le_bytes(needle.try_into().expect("i16 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(2).enumerate() {
        let value = i16::from_le_bytes(chunk.try_into().unwrap());
        if cmp_i64(op, i64::from(value), i64::from(needle)) {
            out.push(idx);
        }
    }
}

fn collect_cmp_i32(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = i32::from_le_bytes(needle.try_into().expect("i32 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(4).enumerate() {
        let value = i32::from_le_bytes(chunk.try_into().unwrap());
        if cmp_i64(op, i64::from(value), i64::from(needle)) {
            out.push(idx);
        }
    }
}

fn collect_cmp_i64(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = i64::from_le_bytes(needle.try_into().expect("i64 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(8).enumerate() {
        let value = i64::from_le_bytes(chunk.try_into().unwrap());
        if cmp_i64(op, value, needle) {
            out.push(idx);
        }
    }
}

fn collect_cmp_i128(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = i128::from_le_bytes(needle.try_into().expect("i128 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(16).enumerate() {
        let value = i128::from_le_bytes(chunk.try_into().unwrap());
        if cmp_i128(op, value, needle) {
            out.push(idx);
        }
    }
}

fn collect_cmp_u128(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = u128::from_le_bytes(needle.try_into().expect("u128 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(16).enumerate() {
        let value = u128::from_le_bytes(chunk.try_into().unwrap());
        if cmp_u128(op, value, needle) {
            out.push(idx);
        }
    }
}

fn collect_cmp_f16(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = f16::from_le_bytes(needle.try_into().expect("f16 needle")).to_f32();
    for (idx, chunk) in payload_bytes.chunks_exact(2).enumerate() {
        let value = f16::from_le_bytes(chunk.try_into().unwrap()).to_f32();
        if cmp_f64(op, f64::from(value), f64::from(needle)) {
            out.push(idx);
        }
    }
}

fn collect_cmp_weight(
    payload_bytes: &[u8],
    op: CmpOp,
    needle: &[u8],
    width: u16,
    encoding: &EdgePayloadEncoding,
    out: &mut Vec<usize>,
) {
    let Ok(decoder) = weight_decoder(width, encoding) else {
        return;
    };
    let Ok(needle) = decode_edge_weight(&decoder, needle) else {
        return;
    };
    for (idx, chunk) in payload_bytes.chunks_exact(usize::from(width)).enumerate() {
        let Ok(value) = decode_edge_weight(&decoder, chunk) else {
            continue;
        };
        if cmp_f64(op, f64::from(value), f64::from(needle)) {
            out.push(idx);
        }
    }
}

fn weight_decoder(
    width: u16,
    encoding: &EdgePayloadEncoding,
) -> Result<PreparedEdgePayloadDecoder, gleaph_graph_kernel::entry::EdgePayloadProfileError> {
    EdgePayloadProfile {
        byte_width: width,
        encoding: encoding.clone(),
    }
    .prepare()
}

fn collect_cmp_f32(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = f32::from_le_bytes(needle.try_into().expect("f32 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(4).enumerate() {
        let value = f32::from_le_bytes(chunk.try_into().unwrap());
        if cmp_f64(op, f64::from(value), f64::from(needle)) {
            out.push(idx);
        }
    }
}

fn collect_cmp_f64(payload_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = f64::from_le_bytes(needle.try_into().expect("f64 needle"));
    for (idx, chunk) in payload_bytes.chunks_exact(8).enumerate() {
        let value = f64::from_le_bytes(chunk.try_into().unwrap());
        if cmp_f64(op, value, needle) {
            out.push(idx);
        }
    }
}

fn collect_cmp_unsigned_scalar(
    payload_bytes: &[u8],
    op: CmpOp,
    needle: &[u8],
    width: usize,
    out: &mut Vec<usize>,
) {
    let needle = read_unsigned_scalar(needle, width);
    for (idx, chunk) in payload_bytes.chunks_exact(width).enumerate() {
        let value = read_unsigned_scalar(chunk, width);
        if cmp_u64(op, value, needle) {
            out.push(idx);
        }
    }
}

fn collect_cmp_fixed_width_bytes(
    payload_bytes: &[u8],
    op: CmpOp,
    needle: &[u8],
    width: usize,
    out: &mut Vec<usize>,
) {
    for (idx, chunk) in payload_bytes.chunks_exact(width).enumerate() {
        if cmp_bytes(op, chunk, needle) {
            out.push(idx);
        }
    }
}

fn read_unsigned_scalar(bytes: &[u8], width: usize) -> u64 {
    match width {
        1 => u64::from(bytes[0]),
        2 => u64::from(u16::from_le_bytes(bytes.try_into().unwrap())),
        4 => u64::from(u32::from_le_bytes(bytes.try_into().unwrap())),
        8 => u64::from_le_bytes(bytes.try_into().unwrap()),
        _ => panic!("unsupported unsigned scalar width {width}"),
    }
}

fn cmp_u8(op: CmpOp, left: u8, right: u8) -> bool {
    cmp_u64(op, u64::from(left), u64::from(right))
}

fn cmp_u16(op: CmpOp, left: u16, right: u16) -> bool {
    cmp_u64(op, u64::from(left), u64::from(right))
}

fn cmp_u32(op: CmpOp, left: u32, right: u32) -> bool {
    cmp_u64(op, u64::from(left), u64::from(right))
}

fn cmp_u64(op: CmpOp, left: u64, right: u64) -> bool {
    match op {
        CmpOp::Lt => left < right,
        CmpOp::Le => left <= right,
        CmpOp::Gt => left > right,
        CmpOp::Ge => left >= right,
        CmpOp::Eq => left == right,
        CmpOp::Ne => left != right,
    }
}

fn cmp_i64(op: CmpOp, left: i64, right: i64) -> bool {
    match op {
        CmpOp::Lt => left < right,
        CmpOp::Le => left <= right,
        CmpOp::Gt => left > right,
        CmpOp::Ge => left >= right,
        CmpOp::Eq => left == right,
        CmpOp::Ne => left != right,
    }
}

fn cmp_u128(op: CmpOp, left: u128, right: u128) -> bool {
    match op {
        CmpOp::Lt => left < right,
        CmpOp::Le => left <= right,
        CmpOp::Gt => left > right,
        CmpOp::Ge => left >= right,
        CmpOp::Eq => left == right,
        CmpOp::Ne => left != right,
    }
}

fn cmp_i128(op: CmpOp, left: i128, right: i128) -> bool {
    match op {
        CmpOp::Lt => left < right,
        CmpOp::Le => left <= right,
        CmpOp::Gt => left > right,
        CmpOp::Ge => left >= right,
        CmpOp::Eq => left == right,
        CmpOp::Ne => left != right,
    }
}

fn cmp_f64(op: CmpOp, left: f64, right: f64) -> bool {
    match op {
        CmpOp::Lt => left < right,
        CmpOp::Le => left <= right,
        CmpOp::Gt => left > right,
        CmpOp::Ge => left >= right,
        CmpOp::Eq => left == right,
        CmpOp::Ne => left != right,
    }
}

fn cmp_bytes(op: CmpOp, left: &[u8], right: &[u8]) -> bool {
    match op {
        CmpOp::Lt => left.cmp(right) == Ordering::Less,
        CmpOp::Le => left.cmp(right) != Ordering::Greater,
        CmpOp::Gt => left.cmp(right) == Ordering::Greater,
        CmpOp::Ge => left.cmp(right) != Ordering::Less,
        CmpOp::Eq => left == right,
        CmpOp::Ne => left != right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::CmpOp;

    #[test]
    fn equal_w1_collects_indices() {
        let kernel = PreparedEdgePayloadBatchKernel::new(1, EdgePayloadEncoding::RawU8);
        let values = [1u8, 2, 1, 3, 1];
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&values, &[1], &mut out);
        assert_eq!(out, vec![0, 2, 4]);
    }

    #[test]
    fn equal_w2_collects_indices() {
        let kernel = PreparedEdgePayloadBatchKernel::new(2, EdgePayloadEncoding::RawU16);
        let values = [1u8, 0, 2, 0, 1, 0];
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&values, &[1, 0], &mut out);
        assert_eq!(out, vec![0, 2]);
    }

    #[test]
    fn equal_w4_collects_indices() {
        let kernel = PreparedEdgePayloadBatchKernel::new(4, EdgePayloadEncoding::RawU32);
        let values = 7u32.to_le_bytes();
        let mut buf = Vec::new();
        buf.extend_from_slice(&values);
        buf.extend_from_slice(&42u32.to_le_bytes());
        buf.extend_from_slice(&values);
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&buf, &7u32.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 2]);
    }

    #[test]
    fn equal_w8_collects_indices() {
        let kernel = PreparedEdgePayloadBatchKernel::new(8, EdgePayloadEncoding::RawU64);
        let values = 9u64.to_le_bytes();
        let mut buf = values.to_vec();
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&values);
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&buf, &9u64.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 2]);
    }

    #[test]
    fn equal_w16_collects_indices() {
        let kernel = PreparedEdgePayloadBatchKernel::new(16, EdgePayloadEncoding::RawU128);
        let values = 5u128.to_le_bytes();
        let mut buf = values.to_vec();
        buf.extend_from_slice(&1u128.to_le_bytes());
        buf.extend_from_slice(&values);
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&buf, &5u128.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 2]);
    }

    #[test]
    fn equal_arbitrary_width_uses_memcmp() {
        let kernel = PreparedEdgePayloadBatchKernel::new(12, EdgePayloadEncoding::RawBytes);
        let needle = [7u8; 12];
        let mut buf = needle.to_vec();
        buf.extend_from_slice(&[0u8; 12]);
        buf.extend_from_slice(&needle);
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&buf, &needle, &mut out);
        assert_eq!(out, vec![0, 2]);
    }

    #[test]
    fn cmp_w1_lt() {
        let kernel = PreparedEdgePayloadBatchKernel::new(1, EdgePayloadEncoding::RawU8);
        let values = [1u8, 2, 3];
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&values, CmpOp::Lt, &[2], &mut out);
        assert_eq!(out, vec![0]);
    }

    #[test]
    fn cmp_w2_gt() {
        let kernel = PreparedEdgePayloadBatchKernel::new(2, EdgePayloadEncoding::RawU16);
        let values = [1u8, 0, 3, 0, 5, 0];
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&values, CmpOp::Gt, &[2, 0], &mut out);
        assert_eq!(out, vec![1, 2]);
    }

    #[test]
    fn cmp_w4_le() {
        let kernel = PreparedEdgePayloadBatchKernel::new(4, EdgePayloadEncoding::RawU32);
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&5u32.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&buf, CmpOp::Le, &3u32.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 1]);
    }

    #[test]
    fn cmp_w8_ge() {
        let kernel = PreparedEdgePayloadBatchKernel::new(8, EdgePayloadEncoding::RawU64);
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&9u64.to_le_bytes());
        buf.extend_from_slice(&5u64.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&buf, CmpOp::Ge, &5u64.to_le_bytes(), &mut out);
        assert_eq!(out, vec![1, 2]);
    }

    #[test]
    fn cmp_w16_eq_delegates_to_equal() {
        let kernel = PreparedEdgePayloadBatchKernel::new(16, EdgePayloadEncoding::RawU128);
        let values = 4u128.to_le_bytes();
        let mut buf = values.to_vec();
        buf.extend_from_slice(&1u128.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&buf, CmpOp::Eq, &values, &mut out);
        assert_eq!(out, vec![0]);
    }

    #[test]
    fn raw_u128_ordering_is_numeric_not_little_endian_memcmp() {
        let kernel = PreparedEdgePayloadBatchKernel::new(16, EdgePayloadEncoding::RawU128);
        let mut buf = Vec::new();
        buf.extend_from_slice(&255u128.to_le_bytes());
        buf.extend_from_slice(&256u128.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&buf, CmpOp::Gt, &255u128.to_le_bytes(), &mut out);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn raw_i128_ordering_is_numeric() {
        let kernel = PreparedEdgePayloadBatchKernel::new(16, EdgePayloadEncoding::RawI128);
        let mut buf = Vec::new();
        buf.extend_from_slice(&(-2i128).to_le_bytes());
        buf.extend_from_slice(&1i128.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&buf, CmpOp::Lt, &0i128.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0]);
    }

    #[test]
    fn f16_ordering_is_decoded_float_ordering() {
        let kernel = PreparedEdgePayloadBatchKernel::new(2, EdgePayloadEncoding::F16);
        let small = f16::from_bits(0x00ff);
        let large = f16::from_bits(0x0100);
        let mut buf = Vec::new();
        buf.extend_from_slice(&small.to_le_bytes());
        buf.extend_from_slice(&large.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&buf, CmpOp::Gt, &small.to_le_bytes(), &mut out);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn weight_ordering_uses_decoded_weight_semantics() {
        let kernel = PreparedEdgePayloadBatchKernel::new(
            2,
            EdgePayloadEncoding::WeightLinearU16 { min: 0.0, max: 1.0 },
        );
        let mut buf = Vec::new();
        buf.extend_from_slice(&255u16.to_le_bytes());
        buf.extend_from_slice(&256u16.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&buf, CmpOp::Gt, &255u16.to_le_bytes(), &mut out);
        assert_eq!(out, vec![1]);
    }
}
