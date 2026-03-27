//! Power management configuration for battery-powered robots.

use serde::{Deserialize, Serialize};

/// Battery state reported by the hardware.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryState {
    /// State of charge (0.0 - 1.0).
    pub soc: f64,
    /// Voltage in volts.
    pub voltage: f64,
    /// Current draw in amps (negative = charging).
    pub current: f64,
    /// Estimated minutes remaining.
    pub minutes_remaining: Option<f64>,
}

/// Three-tier power management thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerConfig {
    /// State-of-charge threshold for warning (default 0.3 = 30%).
    #[serde(default = "default_warning")]
    pub warning_soc: f64,
    /// State-of-charge threshold for return-to-base (default 0.2 = 20%).
    #[serde(default = "default_failsafe")]
    pub failsafe_soc: f64,
    /// State-of-charge threshold for emergency stop (default 0.1 = 10%).
    #[serde(default = "default_emergency")]
    pub emergency_soc: f64,
}

const fn default_warning() -> f64 {
    0.3
}
const fn default_failsafe() -> f64 {
    0.2
}
const fn default_emergency() -> f64 {
    0.1
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            warning_soc: default_warning(),
            failsafe_soc: default_failsafe(),
            emergency_soc: default_emergency(),
        }
    }
}

/// Action to take based on battery level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    /// Normal operation.
    Normal,
    /// Warn operator, conserve power.
    Warning,
    /// Return to charging station.
    ReturnToBase,
    /// Emergency stop and shutdown.
    EmergencyStop,
}

impl PowerConfig {
    /// Evaluate battery state against thresholds.
    #[must_use]
    pub fn evaluate(&self, battery: &BatteryState) -> PowerAction {
        if battery.soc <= self.emergency_soc {
            PowerAction::EmergencyStop
        } else if battery.soc <= self.failsafe_soc {
            PowerAction::ReturnToBase
        } else if battery.soc <= self.warning_soc {
            PowerAction::Warning
        } else {
            PowerAction::Normal
        }
    }
}

/// Evaluate battery state against config and return recommended action.
/// Intended to be called periodically (every 30s) by the worker.
#[must_use]
pub fn check_battery(config: &PowerConfig, battery: &BatteryState) -> PowerAction {
    config.evaluate(battery)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_action_normal() {
        let config = PowerConfig::default();
        let battery = BatteryState {
            soc: 0.8,
            voltage: 12.0,
            current: -1.0,
            minutes_remaining: Some(120.0),
        };
        assert_eq!(config.evaluate(&battery), PowerAction::Normal);
    }

    #[test]
    fn power_action_warning() {
        let config = PowerConfig::default();
        let battery = BatteryState {
            soc: 0.25,
            voltage: 11.5,
            current: -2.0,
            minutes_remaining: Some(30.0),
        };
        assert_eq!(config.evaluate(&battery), PowerAction::Warning);
    }

    #[test]
    fn power_action_return_to_base() {
        let config = PowerConfig::default();
        let battery = BatteryState {
            soc: 0.15,
            voltage: 11.0,
            current: -2.5,
            minutes_remaining: Some(15.0),
        };
        assert_eq!(config.evaluate(&battery), PowerAction::ReturnToBase);
    }

    #[test]
    fn power_action_emergency() {
        let config = PowerConfig::default();
        let battery = BatteryState {
            soc: 0.05,
            voltage: 10.0,
            current: -3.0,
            minutes_remaining: Some(5.0),
        };
        assert_eq!(config.evaluate(&battery), PowerAction::EmergencyStop);
    }

    #[test]
    fn check_battery_full_chain() {
        let config = PowerConfig {
            warning_soc: 0.4,
            failsafe_soc: 0.25,
            emergency_soc: 0.12,
        };

        let normal = BatteryState {
            soc: 0.9,
            voltage: 12.6,
            current: -1.0,
            minutes_remaining: Some(180.0),
        };
        assert_eq!(check_battery(&config, &normal), PowerAction::Normal);

        let warning = BatteryState {
            soc: 0.35,
            voltage: 11.8,
            current: -2.0,
            minutes_remaining: Some(40.0),
        };
        assert_eq!(check_battery(&config, &warning), PowerAction::Warning);

        let failsafe = BatteryState {
            soc: 0.20,
            voltage: 11.2,
            current: -2.5,
            minutes_remaining: Some(18.0),
        };
        assert_eq!(check_battery(&config, &failsafe), PowerAction::ReturnToBase);

        let emergency = BatteryState {
            soc: 0.05,
            voltage: 10.0,
            current: -3.0,
            minutes_remaining: None,
        };
        assert_eq!(check_battery(&config, &emergency), PowerAction::EmergencyStop);
    }
}
