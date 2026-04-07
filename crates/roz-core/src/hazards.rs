use serde::{Deserialize, Serialize};

/// Categories of hazards for systematic safety testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HazardCategory {
    SensorSpoofing,
    PartialSensorFailure,
    UnexpectedHumanPresence,
    PowerLoss,
    NetworkPartition,
    ActuatorJam,
    PayloadShift,
    EnvironmentalChange,
}

/// Inject a simulated hazard into spatial context for testing.
pub fn inject_hazard(category: HazardCategory, context: &mut crate::spatial::WorldState) {
    match category {
        HazardCategory::SensorSpoofing => {
            // Corrupt position data
            for entity in &mut context.entities {
                if let Some(ref mut pos) = entity.position {
                    pos[0] += 999.0; // obviously wrong
                }
            }
        }
        HazardCategory::PartialSensorFailure => {
            // Remove half the entities (simulate sensors going offline)
            let half = context.entities.len() / 2;
            context.entities.truncate(half);
        }
        HazardCategory::UnexpectedHumanPresence => {
            // Add a human entity
            context.entities.push(crate::spatial::EntityState {
                id: "unexpected_human".to_string(),
                kind: "person".to_string(),
                position: Some([1.0, 0.5, 1.7]),
                ..Default::default()
            });
            context.alerts.push(crate::spatial::Alert {
                severity: crate::spatial::AlertSeverity::Critical,
                source: "safety".to_string(),
                message: "Unexpected human detected in workspace".to_string(),
            });
        }
        _ => {
            // Other hazards: add an alert
            context.alerts.push(crate::spatial::Alert {
                severity: crate::spatial::AlertSeverity::Warning,
                source: "hazard_injection".to_string(),
                message: format!("Simulated hazard: {category:?}"),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::{Alert, AlertSeverity, EntityState, WorldState};

    fn make_context_with_entities(n: usize) -> WorldState {
        let entities = (0..n)
            .map(|i| EntityState {
                id: format!("sensor_{i}"),
                kind: "lidar".to_string(),
                position: Some([f64::from(i as u32), 0.0, 0.0]),
                ..Default::default()
            })
            .collect();
        WorldState {
            entities,
            ..Default::default()
        }
    }

    #[test]
    fn inject_sensor_spoofing() {
        let mut ctx = make_context_with_entities(3);
        let original_x = ctx.entities[0].position.unwrap()[0];

        inject_hazard(HazardCategory::SensorSpoofing, &mut ctx);

        // All positions should be corrupted by +999.0 on x-axis
        for entity in &ctx.entities {
            let x = entity.position.unwrap()[0];
            // The offset was applied from the original value
            assert!(
                (x - original_x - 999.0).abs() < f64::EPSILON
                    || (x - 1.0 - 999.0).abs() < f64::EPSILON
                    || (x - 2.0 - 999.0).abs() < f64::EPSILON,
                "position x={x} should be offset by 999.0"
            );
        }
        // No entities removed, no alerts added
        assert_eq!(ctx.entities.len(), 3);
        assert!(ctx.alerts.is_empty());
    }

    #[test]
    fn inject_sensor_spoofing_offsets_each_entity() {
        let mut ctx = make_context_with_entities(2);
        // entity[0] at x=0.0, entity[1] at x=1.0
        inject_hazard(HazardCategory::SensorSpoofing, &mut ctx);
        assert!((ctx.entities[0].position.unwrap()[0] - 999.0).abs() < f64::EPSILON);
        assert!((ctx.entities[1].position.unwrap()[0] - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn inject_human_presence() {
        let mut ctx = make_context_with_entities(2);
        inject_hazard(HazardCategory::UnexpectedHumanPresence, &mut ctx);

        // One human entity added
        assert_eq!(ctx.entities.len(), 3);
        let human = ctx.entities.last().unwrap();
        assert_eq!(human.id, "unexpected_human");
        assert_eq!(human.kind, "person");
        assert_eq!(human.position, Some([1.0, 0.5, 1.7]));

        // Critical alert added
        assert_eq!(ctx.alerts.len(), 1);
        assert_eq!(ctx.alerts[0].severity, AlertSeverity::Critical);
        assert_eq!(ctx.alerts[0].source, "safety");
        assert!(ctx.alerts[0].message.contains("human"));
    }

    #[test]
    fn inject_other_hazards_add_warning_alert() {
        for category in [
            HazardCategory::PowerLoss,
            HazardCategory::NetworkPartition,
            HazardCategory::ActuatorJam,
            HazardCategory::PayloadShift,
            HazardCategory::EnvironmentalChange,
        ] {
            let mut ctx = WorldState::default();
            inject_hazard(category, &mut ctx);
            assert_eq!(ctx.alerts.len(), 1, "expected 1 alert for {category:?}");
            assert_eq!(ctx.alerts[0].severity, AlertSeverity::Warning);
            assert_eq!(ctx.alerts[0].source, "hazard_injection");
        }
    }

    #[test]
    fn inject_partial_sensor_failure() {
        let mut ctx = make_context_with_entities(4);
        inject_hazard(HazardCategory::PartialSensorFailure, &mut ctx);
        assert_eq!(ctx.entities.len(), 2);
    }

    #[test]
    fn hazard_category_serde_roundtrip() {
        let categories = [
            HazardCategory::SensorSpoofing,
            HazardCategory::PartialSensorFailure,
            HazardCategory::UnexpectedHumanPresence,
            HazardCategory::PowerLoss,
            HazardCategory::NetworkPartition,
            HazardCategory::ActuatorJam,
            HazardCategory::PayloadShift,
            HazardCategory::EnvironmentalChange,
        ];

        for category in categories {
            let json = serde_json::to_string(&category).expect("serialize failed");
            let decoded: HazardCategory = serde_json::from_str(&json).expect("deserialize failed");
            assert_eq!(category, decoded, "roundtrip failed for {category:?}");
        }
    }

    #[test]
    fn hazard_category_serde_snake_case() {
        let json = serde_json::to_string(&HazardCategory::SensorSpoofing).unwrap();
        assert_eq!(json, r#""sensor_spoofing""#);

        let json = serde_json::to_string(&HazardCategory::UnexpectedHumanPresence).unwrap();
        assert_eq!(json, r#""unexpected_human_presence""#);
    }

    #[test]
    fn inject_human_presence_preserves_existing_alerts() {
        let mut ctx = WorldState {
            alerts: vec![Alert {
                severity: AlertSeverity::Info,
                source: "monitor".to_string(),
                message: "pre-existing alert".to_string(),
            }],
            ..Default::default()
        };
        inject_hazard(HazardCategory::UnexpectedHumanPresence, &mut ctx);
        assert_eq!(ctx.alerts.len(), 2);
        assert_eq!(ctx.alerts[0].severity, AlertSeverity::Info);
        assert_eq!(ctx.alerts[1].severity, AlertSeverity::Critical);
    }
}
