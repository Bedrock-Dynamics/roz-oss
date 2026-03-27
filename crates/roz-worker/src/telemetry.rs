use std::collections::HashMap;
use std::time::Instant;

use parking_lot::Mutex;
use roz_core::messages::TelemetryMsg;
use roz_nats::subjects::Subjects;
use serde_json::Value;

/// Rate-limited telemetry publisher.
///
/// Tracks per-sensor publish timestamps and enforces a maximum publish rate.
/// The actual NATS publishing is deferred to integration; this struct handles
/// rate limiting, subject construction, and message building.
pub struct TelemetryPublisher {
    max_hz: f64,
    last_publish: Mutex<HashMap<String, Instant>>,
}

impl TelemetryPublisher {
    /// Create a new publisher with the given maximum publish rate (Hz).
    pub fn new(max_hz: f64) -> Self {
        Self {
            max_hz,
            last_publish: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether enough time has passed since the last publish for this sensor.
    ///
    /// If `should_publish` returns true, it also updates the last-publish timestamp.
    pub fn should_publish(&self, sensor_name: &str) -> bool {
        if !self.max_hz.is_finite() || self.max_hz <= 0.0 {
            return false;
        }

        let min_interval = std::time::Duration::from_secs_f64(1.0 / self.max_hz);
        let mut map = self.last_publish.lock();
        let now = Instant::now();

        match map.get(sensor_name) {
            Some(last) if now.duration_since(*last) < min_interval => false,
            _ => {
                map.insert(sensor_name.to_string(), now);
                true
            }
        }
    }

    /// Build a `TelemetryMsg` for the given host, sensor, and data.
    pub fn build_message(host_id: &str, sensor: &str, data: Value) -> TelemetryMsg {
        #[allow(clippy::cast_precision_loss)]
        let ts = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        TelemetryMsg {
            ts,
            stream: format!("{host_id}.{sensor}"),
            data,
        }
    }

    /// Build the NATS subject for telemetry using `roz_nats::subjects::Subjects`.
    pub fn subject(host_id: &str, sensor: &str) -> Result<String, roz_core::errors::RozError> {
        Subjects::telemetry(host_id, sensor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rate_limiting_allows_first_publish() {
        let pub_ = TelemetryPublisher::new(10.0);
        assert!(pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_blocks_rapid_second_publish() {
        let pub_ = TelemetryPublisher::new(10.0); // 10 Hz => 100ms interval
        assert!(pub_.should_publish("imu"));
        // Immediately after, should be blocked
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_independent_per_sensor() {
        let pub_ = TelemetryPublisher::new(10.0);
        assert!(pub_.should_publish("imu"));
        // Different sensor should still be allowed
        assert!(pub_.should_publish("gps"));
        // Same sensor should still be blocked
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_allows_after_interval() {
        let pub_ = TelemetryPublisher::new(1000.0); // 1000 Hz => 1ms interval
        assert!(pub_.should_publish("imu"));
        // Sleep just past the interval
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_zero_hz_blocks_all() {
        let pub_ = TelemetryPublisher::new(0.0);
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_negative_hz_blocks_all() {
        let pub_ = TelemetryPublisher::new(-1.0);
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_nan_hz_blocks_all() {
        let pub_ = TelemetryPublisher::new(f64::NAN);
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn rate_limiting_infinity_hz_blocks_all() {
        let pub_ = TelemetryPublisher::new(f64::INFINITY);
        assert!(!pub_.should_publish("imu"));
    }

    #[test]
    fn subject_construction_correct() {
        let subject = TelemetryPublisher::subject("host1", "imu").unwrap();
        assert_eq!(subject, "telemetry.host1.imu");
    }

    #[test]
    fn subject_construction_validates_tokens() {
        let err = TelemetryPublisher::subject("", "imu");
        assert!(err.is_err());
    }

    #[test]
    #[allow(clippy::float_cmp)] // serde_json round-trips finite f64 exactly
    fn telemetry_msg_serde_roundtrip() {
        let msg = TelemetryPublisher::build_message("host1", "imu", json!({"x": 1.0}));
        assert_eq!(msg.stream, "host1.imu");
        assert_eq!(msg.data, json!({"x": 1.0}));
        assert!(msg.ts > 0.0);

        // Verify serialization roundtrip
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: TelemetryMsg = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.stream, msg.stream);
        assert_eq!(deserialized.data, msg.data);
        assert_eq!(deserialized.ts, msg.ts);
    }
}
