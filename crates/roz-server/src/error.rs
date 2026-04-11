use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[allow(dead_code)]
pub struct AppError(pub roz_core::errors::RozError);

#[allow(dead_code)]
impl AppError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self(roz_core::errors::RozError::Other(anyhow::anyhow!("{}", msg.into())))
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self(roz_core::errors::RozError::Unauthorized(msg.into()))
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self(roz_core::errors::RozError::Validation(msg.into()))
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self(roz_core::errors::RozError::NotFound(msg.into()))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        use roz_core::errors::RozError;
        let (status, message) = match &self.0 {
            RozError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            RozError::NotFound(_) | RozError::SkillNotFound(_) => (StatusCode::NOT_FOUND, self.0.to_string()),
            RozError::Validation(_) | RozError::SkillParse(_) | RozError::BehaviorTree(_) => {
                (StatusCode::BAD_REQUEST, self.0.to_string())
            }
            RozError::LeaseExpired { .. } | RozError::InvalidTransition { .. } | RozError::ConditionViolated(_) => {
                (StatusCode::CONFLICT, self.0.to_string())
            }
            RozError::Recording(_) | RozError::TrustVerification(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, self.0.to_string())
            }
            RozError::ServiceUnavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, self.0.to_string()),
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
        Self(err)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        if matches!(&err, sqlx::Error::PoolTimedOut) {
            tracing::error!("database pool timed out");
            Self(roz_core::errors::RozError::ServiceUnavailable(
                "service temporarily unavailable".into(),
            ))
        } else {
            tracing::error!(error = %err, "database error");
            Self::internal("internal server error")
        }
    }
}
