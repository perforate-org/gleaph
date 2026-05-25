//! SIMD kernels for fixed-width f32 edge vectors.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeVectorMetric {
    Dot,
    L2Squared,
    CosineDistance,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedEdgeVectorKernel {
    dims: usize,
}

impl PreparedEdgeVectorKernel {
    pub(crate) fn new(dims: usize) -> Option<Self> {
        (dims > 0).then_some(Self { dims })
    }

    pub(crate) fn dims(&self) -> usize {
        self.dims
    }

    pub(crate) fn byte_width(&self) -> usize {
        self.dims * 4
    }

    pub(crate) fn score(
        &self,
        edge_value_bytes: &[u8],
        query: &[f32],
        metric: EdgeVectorMetric,
    ) -> Option<f32> {
        if query.len() != self.dims || edge_value_bytes.len() < self.byte_width() {
            return None;
        }
        let bytes = &edge_value_bytes[..self.byte_width()];
        match metric {
            EdgeVectorMetric::Dot => Some(dot_f32_bytes(bytes, query)),
            EdgeVectorMetric::L2Squared => Some(l2_squared_f32_bytes(bytes, query)),
            EdgeVectorMetric::CosineDistance => cosine_distance_f32_bytes(bytes, query),
        }
    }

    pub(crate) fn collect_matching_indices<F>(
        &self,
        value_bytes: &[u8],
        query: &[f32],
        metric: EdgeVectorMetric,
        threshold: f32,
        accepts: F,
        out: &mut Vec<usize>,
    ) where
        F: Fn(f32, f32) -> bool,
    {
        let width = self.byte_width();
        if width == 0 || query.len() != self.dims || value_bytes.len() % width != 0 {
            return;
        }
        for (idx, bytes) in value_bytes.chunks_exact(width).enumerate() {
            let Some(score) = self.score(bytes, query, metric) else {
                continue;
            };
            if accepts(score, threshold) {
                out.push(idx);
            }
        }
    }
}

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn dot_f32_bytes(bytes: &[u8], query: &[f32]) -> f32 {
    bytes
        .chunks_exact(4)
        .zip(query.iter().copied())
        .map(|(chunk, q)| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * q)
        .sum()
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn dot_f32_bytes(bytes: &[u8], query: &[f32]) -> f32 {
    use core::arch::wasm32::{f32x4_add, f32x4_extract_lane, f32x4_mul, f32x4_splat, v128_load};

    let mut acc = f32x4_splat(0.0);
    let chunks = query.len() / 4;
    for i in 0..chunks {
        let offset = i * 16;
        let q_offset = i * 4;
        unsafe {
            let edge = v128_load(bytes[offset..].as_ptr().cast());
            let q = v128_load(query[q_offset..].as_ptr().cast());
            acc = f32x4_add(acc, f32x4_mul(edge, q));
        }
    }
    let mut sum = f32x4_extract_lane::<0>(acc)
        + f32x4_extract_lane::<1>(acc)
        + f32x4_extract_lane::<2>(acc)
        + f32x4_extract_lane::<3>(acc);
    for (chunk, q) in bytes[chunks * 16..]
        .chunks_exact(4)
        .zip(query[chunks * 4..].iter().copied())
    {
        sum += f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * q;
    }
    sum
}

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn l2_squared_f32_bytes(bytes: &[u8], query: &[f32]) -> f32 {
    bytes
        .chunks_exact(4)
        .zip(query.iter().copied())
        .map(|(chunk, q)| {
            let d = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) - q;
            d * d
        })
        .sum()
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn l2_squared_f32_bytes(bytes: &[u8], query: &[f32]) -> f32 {
    use core::arch::wasm32::{
        f32x4_add, f32x4_extract_lane, f32x4_mul, f32x4_splat, f32x4_sub, v128_load,
    };

    let mut acc = f32x4_splat(0.0);
    let chunks = query.len() / 4;
    for i in 0..chunks {
        let offset = i * 16;
        let q_offset = i * 4;
        unsafe {
            let edge = v128_load(bytes[offset..].as_ptr().cast());
            let q = v128_load(query[q_offset..].as_ptr().cast());
            let diff = f32x4_sub(edge, q);
            acc = f32x4_add(acc, f32x4_mul(diff, diff));
        }
    }
    let mut sum = f32x4_extract_lane::<0>(acc)
        + f32x4_extract_lane::<1>(acc)
        + f32x4_extract_lane::<2>(acc)
        + f32x4_extract_lane::<3>(acc);
    for (chunk, q) in bytes[chunks * 16..]
        .chunks_exact(4)
        .zip(query[chunks * 4..].iter().copied())
    {
        let d = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) - q;
        sum += d * d;
    }
    sum
}

fn cosine_distance_f32_bytes(bytes: &[u8], query: &[f32]) -> Option<f32> {
    let (dot, edge_norm2) = dot_and_edge_norm2_f32_bytes(bytes, query);
    let query_norm2: f32 = query.iter().map(|v| v * v).sum();
    if edge_norm2 <= 0.0 || query_norm2 <= 0.0 {
        return None;
    }
    Some(1.0 - dot / (edge_norm2.sqrt() * query_norm2.sqrt()))
}

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn dot_and_edge_norm2_f32_bytes(bytes: &[u8], query: &[f32]) -> (f32, f32) {
    bytes
        .chunks_exact(4)
        .zip(query.iter().copied())
        .fold((0.0, 0.0), |(dot, norm2), (chunk, q)| {
            let edge = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            (dot + edge * q, norm2 + edge * edge)
        })
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn dot_and_edge_norm2_f32_bytes(bytes: &[u8], query: &[f32]) -> (f32, f32) {
    use core::arch::wasm32::{f32x4_add, f32x4_extract_lane, f32x4_mul, f32x4_splat, v128_load};

    let mut dot_acc = f32x4_splat(0.0);
    let mut norm_acc = f32x4_splat(0.0);
    let chunks = query.len() / 4;
    for i in 0..chunks {
        let offset = i * 16;
        let q_offset = i * 4;
        unsafe {
            let edge = v128_load(bytes[offset..].as_ptr().cast());
            let q = v128_load(query[q_offset..].as_ptr().cast());
            dot_acc = f32x4_add(dot_acc, f32x4_mul(edge, q));
            norm_acc = f32x4_add(norm_acc, f32x4_mul(edge, edge));
        }
    }
    let mut dot = f32x4_extract_lane::<0>(dot_acc)
        + f32x4_extract_lane::<1>(dot_acc)
        + f32x4_extract_lane::<2>(dot_acc)
        + f32x4_extract_lane::<3>(dot_acc);
    let mut norm2 = f32x4_extract_lane::<0>(norm_acc)
        + f32x4_extract_lane::<1>(norm_acc)
        + f32x4_extract_lane::<2>(norm_acc)
        + f32x4_extract_lane::<3>(norm_acc);
    for (chunk, q) in bytes[chunks * 16..]
        .chunks_exact(4)
        .zip(query[chunks * 4..].iter().copied())
    {
        let edge = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        dot += edge * q;
        norm2 += edge * edge;
    }
    (dot, norm2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(values: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(values.len() * 4);
        for value in values {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out
    }

    #[test]
    fn scores_dot_and_l2() {
        let kernel = PreparedEdgeVectorKernel::new(4).expect("kernel");
        let edge = bytes(&[1.0, 2.0, 3.0, 4.0]);
        let query = [2.0, 0.5, 1.0, 1.5];

        assert_eq!(
            kernel.score(&edge, &query, EdgeVectorMetric::Dot),
            Some(12.0)
        );
        assert_eq!(
            kernel.score(&edge, &query, EdgeVectorMetric::L2Squared),
            Some(13.5)
        );
    }

    #[test]
    fn collects_l2_threshold_indices() {
        let kernel = PreparedEdgeVectorKernel::new(4).expect("kernel");
        let mut values = Vec::new();
        values.extend_from_slice(&bytes(&[1.0, 1.0, 1.0, 1.0]));
        values.extend_from_slice(&bytes(&[2.0, 2.0, 2.0, 2.0]));
        values.extend_from_slice(&bytes(&[9.0, 9.0, 9.0, 9.0]));
        let mut out = Vec::new();

        kernel.collect_matching_indices(
            &values,
            &[1.0, 1.0, 1.0, 1.0],
            EdgeVectorMetric::L2Squared,
            4.0,
            |score, threshold| score <= threshold,
            &mut out,
        );

        assert_eq!(out, vec![0, 1]);
    }

    #[test]
    fn cosine_distance_identical_vector_is_zero() {
        let kernel = PreparedEdgeVectorKernel::new(4).expect("kernel");
        let edge = bytes(&[1.0, 2.0, 3.0, 4.0]);
        let score = kernel
            .score(
                &edge,
                &[1.0, 2.0, 3.0, 4.0],
                EdgeVectorMetric::CosineDistance,
            )
            .expect("score");

        assert!(score.abs() < 1e-6);
    }
}
