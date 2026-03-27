use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Status of a host as tracked by the `HostService` virtual object
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HostStatus {
    Online {
        since: DateTime<Utc>,
        current_task: Option<uuid::Uuid>,
        capabilities: Vec<String>,
    },
    Offline {
        last_seen: DateTime<Utc>,
    },
    Busy {
        task_id: uuid::Uuid,
        started_at: DateTime<Utc>,
    },
}

impl HostStatus {
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Online { current_task: None, .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // HostStatus all 3 variants serialize correctly
    // -----------------------------------------------------------------------

    #[test]
    fn host_status_online_tag() {
        let status = HostStatus::Online {
            since: Utc::now(),
            current_task: None,
            capabilities: vec!["arm".to_string(), "gripper".to_string()],
        };
        let json_str = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "online");
        assert!(value["capabilities"].as_array().unwrap().len() == 2);
    }

    #[test]
    fn host_status_offline_tag() {
        let status = HostStatus::Offline { last_seen: Utc::now() };
        let json_str = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "offline");
    }

    #[test]
    fn host_status_busy_tag() {
        let task_id = Uuid::new_v4();
        let status = HostStatus::Busy {
            task_id,
            started_at: Utc::now(),
        };
        let json_str = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(value["status"], "busy");
        assert_eq!(value["task_id"], task_id.to_string());
    }

    // -----------------------------------------------------------------------
    // is_available returns true only for Online with no current task
    // -----------------------------------------------------------------------

    #[test]
    fn is_available_online_no_task() {
        let status = HostStatus::Online {
            since: Utc::now(),
            current_task: None,
            capabilities: vec![],
        };
        assert!(status.is_available());
    }

    #[test]
    fn is_available_online_with_task() {
        let status = HostStatus::Online {
            since: Utc::now(),
            current_task: Some(Uuid::new_v4()),
            capabilities: vec![],
        };
        assert!(!status.is_available());
    }

    #[test]
    fn is_available_offline() {
        let status = HostStatus::Offline { last_seen: Utc::now() };
        assert!(!status.is_available());
    }

    #[test]
    fn is_available_busy() {
        let status = HostStatus::Busy {
            task_id: Uuid::new_v4(),
            started_at: Utc::now(),
        };
        assert!(!status.is_available());
    }

    // -----------------------------------------------------------------------
    // Serde round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn host_status_online_serde_roundtrip() {
        let status = HostStatus::Online {
            since: Utc::now(),
            current_task: Some(Uuid::new_v4()),
            capabilities: vec!["sensor".to_string()],
        };
        let json_str = serde_json::to_string(&status).unwrap();
        let deser: HostStatus = serde_json::from_str(&json_str).unwrap();
        // Verify variant matches
        assert!(matches!(deser, HostStatus::Online { .. }));
    }

    #[test]
    fn host_status_offline_serde_roundtrip() {
        let status = HostStatus::Offline { last_seen: Utc::now() };
        let json_str = serde_json::to_string(&status).unwrap();
        let deser: HostStatus = serde_json::from_str(&json_str).unwrap();
        assert!(matches!(deser, HostStatus::Offline { .. }));
    }

    #[test]
    fn host_status_busy_serde_roundtrip() {
        let status = HostStatus::Busy {
            task_id: Uuid::new_v4(),
            started_at: Utc::now(),
        };
        let json_str = serde_json::to_string(&status).unwrap();
        let deser: HostStatus = serde_json::from_str(&json_str).unwrap();
        assert!(matches!(deser, HostStatus::Busy { .. }));
    }
}
