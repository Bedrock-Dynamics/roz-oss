use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct ApiResponse<T> {
    pub data: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

#[allow(dead_code)]
impl<T: Serialize> ApiResponse<T> {
    pub const fn ok(data: T) -> Json<Self> {
        Json(Self { data, meta: None })
    }

    pub const fn with_meta(data: T, meta: serde_json::Value) -> Json<Self> {
        Json(Self { data, meta: Some(meta) })
    }
}
