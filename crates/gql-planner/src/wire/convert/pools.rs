use gleaph_gql::ast::{Expr, OrderByClause};
use gleaph_gql::types::LabelExpr;
use rkyv::rancor;

pub(super) fn rkyv_encode_expr(value: &Expr) -> Result<Vec<u8>, String> {
    rkyv::to_bytes::<rancor::Error>(value)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

pub(super) fn rkyv_decode_expr(bytes: &[u8]) -> Result<Expr, String> {
    rkyv::from_bytes::<Expr, rancor::Error>(bytes).map_err(|e| e.to_string())
}

pub(super) fn rkyv_encode_label_expr(value: &LabelExpr) -> Result<Vec<u8>, String> {
    rkyv::to_bytes::<rancor::Error>(value)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

pub(super) fn rkyv_decode_label_expr(bytes: &[u8]) -> Result<LabelExpr, String> {
    rkyv::from_bytes::<LabelExpr, rancor::Error>(bytes).map_err(|e| e.to_string())
}

pub(super) fn rkyv_encode_order_by(value: &OrderByClause) -> Result<Vec<u8>, String> {
    rkyv::to_bytes::<rancor::Error>(value)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

pub(super) fn rkyv_decode_order_by(bytes: &[u8]) -> Result<OrderByClause, String> {
    rkyv::from_bytes::<OrderByClause, rancor::Error>(bytes).map_err(|e| e.to_string())
}
