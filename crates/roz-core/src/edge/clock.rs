//! Clock synchronization configuration.

use serde::{Deserialize, Serialize};

/// Clock sync method for multi-sensor fusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockSyncMethod {
    /// NTP via chrony (default, sub-us on wired LAN).
    Ntp,
    /// PTP / IEEE 1588 via linuxptp (for `GigE` sensors).
    Ptp,
    /// No external sync (use system monotonic clock).
    Monotonic,
}

/// Clock configuration for the edge device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClockConfig {
    /// Primary sync method.
    #[serde(default = "default_method")]
    pub method: ClockSyncMethod,
    /// NTP server address (default: pool.ntp.org).
    pub ntp_server: Option<String>,
    /// PTP interface (e.g., "eth0").
    pub ptp_interface: Option<String>,
}

const fn default_method() -> ClockSyncMethod {
    ClockSyncMethod::Ntp
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            method: default_method(),
            ntp_server: None,
            ptp_interface: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_config_default() {
        let config = ClockConfig::default();
        assert_eq!(config.method, ClockSyncMethod::Ntp);
    }
}
