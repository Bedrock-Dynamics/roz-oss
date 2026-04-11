//! Validated pagination extractor for REST list endpoints.
//!
//! Replaces per-route `PaginationParams` structs with a single shared
//! extractor that validates `limit` (1..=100, default 50) and `offset`
//! (>= 0, default 0), returning 400 with descriptive errors on invalid input.

use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::AppError;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 100;

/// Validated pagination parameters extracted from query string.
///
/// Validates `limit` (1..=100, default 50) and `offset` (>= 0, default 0).
/// Returns 400 with a descriptive error message on invalid input.
#[derive(Debug)]
pub struct ValidatedPagination {
    pub limit: i64,
    pub offset: i64,
}

#[derive(Deserialize)]
struct RawPagination {
    limit: Option<i64>,
    offset: Option<i64>,
}

impl<S> FromRequestParts<S> for ValidatedPagination
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(raw) = Query::<RawPagination>::from_request_parts(parts, state)
            .await
            .map_err(|e| AppError::bad_request(e.to_string()).into_response())?;

        let limit = raw.limit.unwrap_or(DEFAULT_LIMIT);
        let offset = raw.offset.unwrap_or(0);

        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(AppError::bad_request(format!(
                "limit must be between 1 and {MAX_LIMIT}, got {limit}"
            ))
            .into_response());
        }

        if offset < 0 {
            return Err(
                AppError::bad_request(format!("offset must be >= 0, got {offset}")).into_response()
            );
        }

        Ok(Self { limit, offset })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a ValidatedPagination from raw Option values,
    /// simulating what FromRequestParts would do after deserialization.
    fn validate(limit: Option<i64>, offset: Option<i64>) -> Result<ValidatedPagination, String> {
        let limit_val = limit.unwrap_or(DEFAULT_LIMIT);
        let offset_val = offset.unwrap_or(0);

        if !(1..=MAX_LIMIT).contains(&limit_val) {
            return Err(format!(
                "limit must be between 1 and {MAX_LIMIT}, got {limit_val}"
            ));
        }

        if offset_val < 0 {
            return Err(format!("offset must be >= 0, got {offset_val}"));
        }

        Ok(ValidatedPagination {
            limit: limit_val,
            offset: offset_val,
        })
    }

    #[test]
    fn defaults_when_no_params() {
        let p = validate(None, None).unwrap();
        assert_eq!(p.limit, 50);
        assert_eq!(p.offset, 0);
    }

    #[test]
    fn explicit_valid_values() {
        let p = validate(Some(10), Some(20)).unwrap();
        assert_eq!(p.limit, 10);
        assert_eq!(p.offset, 20);
    }

    #[test]
    fn boundary_min_limit() {
        let p = validate(Some(1), None).unwrap();
        assert_eq!(p.limit, 1);
    }

    #[test]
    fn boundary_max_limit() {
        let p = validate(Some(100), None).unwrap();
        assert_eq!(p.limit, 100);
    }

    #[test]
    fn limit_zero_rejected() {
        let err = validate(Some(0), None).unwrap_err();
        assert!(err.contains("limit must be between 1 and 100, got 0"));
    }

    #[test]
    fn limit_over_max_rejected() {
        let err = validate(Some(101), None).unwrap_err();
        assert!(err.contains("limit must be between 1 and 100, got 101"));
    }

    #[test]
    fn negative_limit_rejected() {
        let err = validate(Some(-1), None).unwrap_err();
        assert!(err.contains("limit must be between 1 and 100, got -1"));
    }

    #[test]
    fn negative_offset_rejected() {
        let err = validate(None, Some(-1)).unwrap_err();
        assert!(err.contains("offset must be >= 0, got -1"));
    }

    #[test]
    fn zero_offset_valid() {
        let p = validate(None, Some(0)).unwrap();
        assert_eq!(p.offset, 0);
    }
}
