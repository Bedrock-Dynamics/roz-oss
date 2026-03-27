use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// LeaseState
// ---------------------------------------------------------------------------

/// The computed state of a capability lease at a given point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Active,
    Expired,
    Released,
}

// ---------------------------------------------------------------------------
// CapabilityLease
// ---------------------------------------------------------------------------

/// A time-bounded, exclusive lease on a named resource (e.g. a hardware
/// actuator, a simulation slot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityLease {
    pub id: Uuid,
    pub tenant_id: String,
    pub host_id: String,
    pub resource: String,
    pub holder_id: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

impl CapabilityLease {
    /// Compute the lease state at the given instant.
    pub fn state(&self, now: DateTime<Utc>) -> LeaseState {
        if self.released_at.is_some() {
            LeaseState::Released
        } else if now >= self.expires_at {
            LeaseState::Expired
        } else {
            LeaseState::Active
        }
    }

    /// Returns `true` only if the lease is [`LeaseState::Active`] at the given
    /// instant.
    pub fn is_valid(&self, now: DateTime<Utc>) -> bool {
        self.state(now) == LeaseState::Active
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn sample_lease(expires_in: Duration, released_at: Option<DateTime<Utc>>) -> CapabilityLease {
        let now = Utc::now();
        CapabilityLease {
            id: Uuid::new_v4(),
            tenant_id: "tenant-acme".into(),
            host_id: "host-alpha".into(),
            resource: "arm/gripper".into(),
            holder_id: "worker-42".into(),
            acquired_at: now,
            expires_at: now + expires_in,
            released_at,
        }
    }

    // -----------------------------------------------------------------------
    // LeaseState serde
    // -----------------------------------------------------------------------

    #[test]
    fn lease_state_serde_roundtrip() {
        let states = [LeaseState::Active, LeaseState::Expired, LeaseState::Released];
        for state in &states {
            let json = serde_json::to_string(state).unwrap();
            let back: LeaseState = serde_json::from_str(&json).unwrap();
            assert_eq!(*state, back);
        }
    }

    #[test]
    fn lease_state_serializes_to_snake_case() {
        assert_eq!(serde_json::to_string(&LeaseState::Active).unwrap(), "\"active\"");
        assert_eq!(serde_json::to_string(&LeaseState::Expired).unwrap(), "\"expired\"");
        assert_eq!(serde_json::to_string(&LeaseState::Released).unwrap(), "\"released\"");
    }

    // -----------------------------------------------------------------------
    // state() / is_valid()
    // -----------------------------------------------------------------------

    #[test]
    fn active_lease() {
        let lease = sample_lease(Duration::hours(1), None);
        let now = Utc::now();
        assert_eq!(lease.state(now), LeaseState::Active);
        assert!(lease.is_valid(now));
    }

    #[test]
    fn expired_lease() {
        let lease = sample_lease(Duration::hours(-1), None);
        let now = Utc::now();
        assert_eq!(lease.state(now), LeaseState::Expired);
        assert!(!lease.is_valid(now));
    }

    #[test]
    fn released_lease() {
        let lease = sample_lease(Duration::hours(1), Some(Utc::now()));
        let now = Utc::now();
        assert_eq!(lease.state(now), LeaseState::Released);
        assert!(!lease.is_valid(now));
    }

    #[test]
    fn released_takes_precedence_over_expired() {
        // Even when expires_at is in the past, released_at wins.
        let lease = sample_lease(Duration::hours(-1), Some(Utc::now()));
        let now = Utc::now();
        assert_eq!(lease.state(now), LeaseState::Released);
        assert!(!lease.is_valid(now));
    }

    // -----------------------------------------------------------------------
    // CapabilityLease serde
    // -----------------------------------------------------------------------

    #[test]
    fn capability_lease_serde_roundtrip() {
        let lease = sample_lease(Duration::hours(1), None);
        let json = serde_json::to_string(&lease).unwrap();
        let back: CapabilityLease = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, lease.id);
        assert_eq!(back.tenant_id, lease.tenant_id);
        assert_eq!(back.host_id, lease.host_id);
        assert_eq!(back.resource, lease.resource);
        assert_eq!(back.holder_id, lease.holder_id);
        assert_eq!(back.acquired_at, lease.acquired_at);
        assert_eq!(back.expires_at, lease.expires_at);
        assert_eq!(back.released_at, lease.released_at);
    }

    #[test]
    fn capability_lease_with_released_at_serde_roundtrip() {
        let lease = sample_lease(Duration::hours(1), Some(Utc::now()));
        let json = serde_json::to_string(&lease).unwrap();
        let back: CapabilityLease = serde_json::from_str(&json).unwrap();
        assert_eq!(back.released_at, lease.released_at);
    }
}
