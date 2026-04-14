use thiserror::Error;

#[derive(Debug, Error)]
pub enum RozError {
    #[error("invalid state transition: {from} -> {to}")]
    InvalidTransition { from: String, to: String },

    #[error("lease expired: {id}")]
    LeaseExpired { id: String },

    #[error("lease not held: {resource}")]
    LeaseNotHeld { resource: String },

    #[error("invalid frame: {0}")]
    InvalidFrame(String),

    #[error("invalid unit: {0}")]
    InvalidUnit(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("validation error: {0}")]
    Validation(String),

    // Phase 4 — Skills, BT, Sim-to-Real, DeviceTrust
    #[error("skill parse error: {0}")]
    SkillParse(String),

    #[error("skill not found: {0}")]
    SkillNotFound(String),

    #[error("behavior tree error: {0}")]
    BehaviorTree(String),

    #[error("condition violated: {0}")]
    ConditionViolated(String),

    #[error("recording error: {0}")]
    Recording(String),

    #[error("trust verification error: {0}")]
    TrustVerification(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, RozError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages() {
        let err = RozError::InvalidTransition {
            from: "Accepted".into(),
            to: "Completed".into(),
        };
        assert_eq!(err.to_string(), "invalid state transition: Accepted -> Completed");

        let err = RozError::LeaseExpired { id: "lease-1".into() };
        assert_eq!(err.to_string(), "lease expired: lease-1");
    }

    #[test]
    fn result_type_alias_works() {
        let ok: Result<i32> = Ok(42);
        assert!(ok.is_ok());

        let err: Result<i32> = Err(RozError::Validation("bad".into()));
        assert!(err.is_err());
    }

    #[test]
    fn phase4_error_variants() {
        let errors: Vec<RozError> = vec![
            RozError::SkillParse("bad frontmatter".into()),
            RozError::SkillNotFound("nonexistent".into()),
            RozError::BehaviorTree("invalid node".into()),
            RozError::ConditionViolated("force exceeded".into()),
            RozError::Recording("corrupt MCAP".into()),
            RozError::TrustVerification("sig mismatch".into()),
        ];
        for err in &errors {
            assert!(!err.to_string().is_empty());
        }
    }
}
