use axum::http::{HeaderValue, Request};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

pub async fn request_id_middleware(mut req: Request<axum::body::Body>, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| Uuid::new_v4().to_string(), String::from);

    // Record in the current tracing span so it appears in OTel traces
    tracing::Span::current().record("request_id", &request_id);

    req.headers_mut()
        .insert("x-request-id", HeaderValue::from_str(&request_id).unwrap());

    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert("x-request-id", HeaderValue::from_str(&request_id).unwrap());
    response
}
