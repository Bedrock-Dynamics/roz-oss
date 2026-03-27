use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ClockQuality
// ---------------------------------------------------------------------------

/// Describes the synchronisation quality of a host's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockQuality {
    GpsSynced,
    NtpSynced,
    FreeRunning,
    Unknown,
}

// ---------------------------------------------------------------------------
// TimebaseInfo
// ---------------------------------------------------------------------------

/// Snapshot of a host's clock state relative to a reference source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimebaseInfo {
    pub host_id: String,
    pub quality: ClockQuality,
    /// Estimated offset from reference clock in microseconds.
    pub offset_us: i64,
    /// Estimated drift rate in parts-per-million.
    pub drift_ppm: f64,
    /// When the clock was last synchronised.
    pub last_sync: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// MonotonicTimestamp
// ---------------------------------------------------------------------------

/// A monotonic timestamp tied to a specific host's boot epoch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MonotonicTimestamp {
    pub nanos_since_boot: u64,
    pub host_id: String,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // ClockQuality
    // -----------------------------------------------------------------------

    #[test]
    fn clock_quality_serde_roundtrip() {
        let variants = [
            ClockQuality::GpsSynced,
            ClockQuality::NtpSynced,
            ClockQuality::FreeRunning,
            ClockQuality::Unknown,
        ];
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let back: ClockQuality = serde_json::from_str(&json).unwrap();
            assert_eq!(*variant, back);
        }
    }

    #[test]
    fn clock_quality_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&ClockQuality::GpsSynced).unwrap(),
            "\"gps_synced\""
        );
        assert_eq!(
            serde_json::to_string(&ClockQuality::NtpSynced).unwrap(),
            "\"ntp_synced\""
        );
        assert_eq!(
            serde_json::to_string(&ClockQuality::FreeRunning).unwrap(),
            "\"free_running\""
        );
        assert_eq!(serde_json::to_string(&ClockQuality::Unknown).unwrap(), "\"unknown\"");
    }

    // -----------------------------------------------------------------------
    // TimebaseInfo
    // -----------------------------------------------------------------------

    #[test]
    fn timebase_info_serde_roundtrip() {
        let info = TimebaseInfo {
            host_id: "host-alpha".into(),
            quality: ClockQuality::GpsSynced,
            offset_us: -42,
            drift_ppm: 0.5,
            last_sync: Utc::now(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: TimebaseInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.host_id, info.host_id);
        assert_eq!(back.quality, info.quality);
        assert_eq!(back.offset_us, info.offset_us);
        assert!((back.drift_ppm - info.drift_ppm).abs() < f64::EPSILON);
        assert_eq!(back.last_sync, info.last_sync);
    }

    // -----------------------------------------------------------------------
    // MonotonicTimestamp
    // -----------------------------------------------------------------------

    #[test]
    fn monotonic_timestamp_serde_roundtrip() {
        let ts = MonotonicTimestamp {
            nanos_since_boot: 123_456_789_000,
            host_id: "host-beta".into(),
        };
        let json = serde_json::to_string(&ts).unwrap();
        let back: MonotonicTimestamp = serde_json::from_str(&json).unwrap();
        assert_eq!(ts, back);
    }
}
