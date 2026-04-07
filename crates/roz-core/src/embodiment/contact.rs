use serde::{Deserialize, Serialize};

/// Tactile sensor array data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TactileArrayData {
    pub sensor_id: String,
    pub pressures: Vec<f64>,
    pub contacts: Vec<bool>,
    pub resolution: (u32, u32),
    pub timestamp_ns: u64,
}

/// Summary of contact state for the tick contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactState {
    pub in_contact: bool,
    pub contact_force: Option<f64>,
    pub contact_location: Option<String>,
    pub slip_detected: bool,
    pub contact_confidence: f64,
}

/// Force envelope for a specific link or contact zone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactForceEnvelope {
    pub link_name: String,
    pub max_normal_force_n: f64,
    pub max_shear_force_n: f64,
    pub max_force_rate_n_per_s: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tactile_array_serde() {
        let data = TactileArrayData {
            sensor_id: "fingertip_left".into(),
            pressures: vec![0.0, 0.1, 0.5, 0.3, 0.0, 0.0, 0.2, 0.1, 0.0],
            contacts: vec![false, true, true, true, false, false, true, true, false],
            resolution: (3, 3),
            timestamp_ns: 42_000_000,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TactileArrayData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, back);
        assert_eq!(back.pressures.len(), 9);
        assert_eq!(back.contacts.len(), 9);
    }

    #[test]
    fn contact_state_serde() {
        let state = ContactState {
            in_contact: true,
            contact_force: Some(5.2),
            contact_location: Some("fingertip_left".into()),
            slip_detected: false,
            contact_confidence: 0.92,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: ContactState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn contact_state_no_contact_serde() {
        let state = ContactState {
            in_contact: false,
            contact_force: None,
            contact_location: None,
            slip_detected: false,
            contact_confidence: 0.0,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: ContactState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
        assert!(!back.in_contact);
    }

    #[test]
    fn contact_force_envelope_serde() {
        let envelope = ContactForceEnvelope {
            link_name: "gripper_finger_left".into(),
            max_normal_force_n: 20.0,
            max_shear_force_n: 5.0,
            max_force_rate_n_per_s: 100.0,
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: ContactForceEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, back);
    }
}
