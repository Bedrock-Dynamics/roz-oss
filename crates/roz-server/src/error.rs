use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Application error wrapper used across REST handlers.
///
/// Normally wraps a `roz_core::errors::RozError` and maps it to an HTTP
/// response via `IntoResponse`. A small escape hatch (`wire_override`) lets
/// callers emit a **fixed** status/body pair without the `RozError` Display
/// prefix, which is required for security-sensitive error shapes where the
/// exact wire bytes are part of the contract (e.g. trust-rejection 409).
#[allow(dead_code)]
pub struct AppError {
    pub inner: roz_core::errors::RozError,
    /// When `Some`, `into_response` emits exactly this status + body instead
    /// of the default `RozError`-derived mapping. Used for trust rejection so
    /// the wire message is literally `host_trust_posture_not_satisfied` with
    /// no prefix from thiserror's `#[error("condition violated: {0}")]`.
    wire_override: Option<(StatusCode, &'static str)>,
}

#[allow(dead_code)]
impl AppError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            inner: roz_core::errors::RozError::Other(anyhow::anyhow!("{}", msg.into())),
            wire_override: None,
        }
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            inner: roz_core::errors::RozError::Unauthorized(msg.into()),
            wire_override: None,
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            inner: roz_core::errors::RozError::Validation(msg.into()),
            wire_override: None,
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            inner: roz_core::errors::RozError::NotFound(msg.into()),
            wire_override: None,
        }
    }

    /// Build a trust-rejection error with a fixed opaque wire shape.
    ///
    /// Produces HTTP `409 Conflict` with body `{"error":"host_trust_posture_not_satisfied"}`.
    /// Evaluator detail (firmware version, attestation age, posture) MUST be
    /// logged server-side via `tracing::warn!` — never leaked here.
    #[must_use]
    pub fn trust_rejected() -> Self {
        Self {
            // Keep a RozError for any internal branches that match on it; the
            // wire shape comes from the override, not from this variant.
            inner: roz_core::errors::RozError::ConditionViolated("host_trust_posture_not_satisfied".into()),
            wire_override: Some((StatusCode::CONFLICT, "host_trust_posture_not_satisfied")),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        use roz_core::errors::RozError;

        if let Some((status, message)) = self.wire_override {
            return (status, Json(json!({ "error": message }))).into_response();
        }

        let (status, message) = match &self.inner {
            RozError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            RozError::NotFound(_) | RozError::SkillNotFound(_) => (StatusCode::NOT_FOUND, self.inner.to_string()),
            RozError::Validation(_) | RozError::SkillParse(_) | RozError::BehaviorTree(_) => {
                (StatusCode::BAD_REQUEST, self.inner.to_string())
            }
            RozError::LeaseExpired { .. } | RozError::InvalidTransition { .. } | RozError::ConditionViolated(_) => {
                (StatusCode::CONFLICT, self.inner.to_string())
            }
            RozError::Recording(_) | RozError::TrustVerification(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, self.inner.to_string())
            }
            RozError::ServiceUnavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, self.inner.to_string()),
            RozError::InvalidFrame(_)
            | RozError::InvalidUnit(_)
            | RozError::LeaseNotHeld { .. }
            | RozError::Other(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string()),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<roz_core::errors::RozError> for AppError {
    fn from(err: roz_core::errors::RozError) -> Self {
        Self {
            inner: err,
            wire_override: None,
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        if matches!(&err, sqlx::Error::PoolTimedOut) {
            tracing::error!("database pool timed out");
            Self {
                inner: roz_core::errors::RozError::ServiceUnavailable("service temporarily unavailable".into()),
                wire_override: None,
            }
        } else {
            tracing::error!(error = %err, "database error");
            Self::internal("internal server error")
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    /// `AppError::trust_rejected()` must produce exactly HTTP 409 with body
    /// `{"error":"host_trust_posture_not_satisfied"}` — no extra fields, no
    /// RozError Display prefix (locked by CONTEXT D-04).
    #[tokio::test]
    async fn trust_rejected_maps_to_409_with_exact_body() {
        let resp = AppError::trust_rejected().into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("read body");
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse body");

        // Exact shape: one field `error`, value is the fixed opaque string.
        assert_eq!(body, serde_json::json!({"error": "host_trust_posture_not_satisfied"}));
    }

    /// Regression: the existing `bad_request` path must still emit the
    /// thiserror-prefixed message (unchanged behavior from the newtype-struct
    /// refactor).
    #[tokio::test]
    async fn bad_request_preserves_display_prefix() {
        let resp = AppError::bad_request("nope").into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"], "validation error: nope");
    }
}
