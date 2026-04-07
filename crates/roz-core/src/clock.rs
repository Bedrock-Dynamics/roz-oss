use serde::{Deserialize, Serialize};

/// Which clock domain a timestamp belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockDomain {
    Monotonic,
    WallClock,
    SensorClock,
}

/// A freshness contract for a data source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessContract {
    pub source: String,
    pub max_age_ms: u64,
    pub clock_domain: ClockDomain,
}

/// Monotonic timestamp for the Copper hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MonotonicTimestamp {
    pub ns: u64,
}

impl MonotonicTimestamp {
    pub const fn new(ns: u64) -> Self {
        Self { ns }
    }

    pub const fn as_nanos(self) -> u64 {
        self.ns
    }

    pub const fn as_micros(self) -> u64 {
        self.ns / 1_000
    }

    pub const fn as_millis(self) -> u64 {
        self.ns / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_domain_serde_roundtrip() {
        let variants = [ClockDomain::Monotonic, ClockDomain::WallClock, ClockDomain::SensorClock];
        for domain in variants {
            let json = serde_json::to_string(&domain).unwrap();
            let back: ClockDomain = serde_json::from_str(&json).unwrap();
            assert_eq!(domain, back);
        }
        // Verify snake_case serialization
        assert_eq!(
            serde_json::to_string(&ClockDomain::WallClock).unwrap(),
            "\"wall_clock\""
        );
        assert_eq!(
            serde_json::to_string(&ClockDomain::SensorClock).unwrap(),
            "\"sensor_clock\""
        );
        assert_eq!(serde_json::to_string(&ClockDomain::Monotonic).unwrap(), "\"monotonic\"");
    }

    #[test]
    fn freshness_contract_serde() {
        let contract = FreshnessContract {
            source: "joint_encoder".to_string(),
            max_age_ms: 50,
            clock_domain: ClockDomain::Monotonic,
        };
        let json = serde_json::to_string(&contract).unwrap();
        let back: FreshnessContract = serde_json::from_str(&json).unwrap();
        assert_eq!(contract, back);
        assert_eq!(back.source, "joint_encoder");
        assert_eq!(back.max_age_ms, 50);
        assert_eq!(back.clock_domain, ClockDomain::Monotonic);
    }

    #[test]
    fn monotonic_timestamp_conversions() {
        let ts = MonotonicTimestamp::new(1_500_000_000);
        assert_eq!(ts.as_nanos(), 1_500_000_000);
        assert_eq!(ts.as_micros(), 1_500_000);
        assert_eq!(ts.as_millis(), 1_500);
    }

    #[test]
    fn monotonic_timestamp_serde() {
        let ts = MonotonicTimestamp::new(42_000_000);
        let json = serde_json::to_string(&ts).unwrap();
        let back: MonotonicTimestamp = serde_json::from_str(&json).unwrap();
        assert_eq!(ts, back);
    }

    #[test]
    fn monotonic_timestamp_ordering() {
        let earlier = MonotonicTimestamp::new(100);
        let later = MonotonicTimestamp::new(200);
        assert!(earlier < later);
        assert_eq!(earlier, MonotonicTimestamp::new(100));
    }
}
