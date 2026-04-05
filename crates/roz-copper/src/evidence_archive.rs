use std::io;
use std::path::{Path, PathBuf};

use roz_core::controller::evidence::ControllerEvidenceBundle;

use crate::replay::ReplayResult;

/// File-backed archive for finalized controller evidence bundles.
#[derive(Debug, Clone)]
pub struct EvidenceArchive {
    dir: PathBuf,
}

impl EvidenceArchive {
    #[must_use]
    pub fn new(project_dir: &Path) -> Self {
        Self {
            dir: project_dir.join(".roz").join("controller-evidence"),
        }
    }

    #[must_use]
    pub fn path_for(&self, bundle_id: &str) -> PathBuf {
        self.dir.join(format!("{bundle_id}.json"))
    }

    pub fn save(&self, bundle: &ControllerEvidenceBundle) -> io::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path_for(&bundle.bundle_id);
        let json =
            serde_json::to_string_pretty(bundle).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    pub fn load(&self, bundle_id: &str) -> io::Result<ControllerEvidenceBundle> {
        let path = self.path_for(bundle_id);
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub fn save_replay_result(&self, result: &ReplayResult) -> io::Result<PathBuf> {
        self.save(&result.evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use roz_core::controller::artifact::ExecutionMode;
    use roz_core::controller::evidence::StabilitySummary;
    use roz_core::session::snapshot::FreshnessState;

    fn sample_bundle() -> ControllerEvidenceBundle {
        ControllerEvidenceBundle {
            bundle_id: "ev-001".into(),
            controller_id: "ctrl-001".into(),
            ticks_run: 12,
            rejection_count: 1,
            limit_clamp_count: 2,
            rate_clamp_count: 3,
            position_limit_stop_count: 0,
            epoch_interrupt_count: 0,
            trap_count: 0,
            watchdog_near_miss_count: 0,
            channels_touched: vec!["shoulder".into()],
            channels_untouched: vec![],
            config_reads: 1,
            tick_latency_p50: 100.into(),
            tick_latency_p95: 200.into(),
            tick_latency_p99: 300.into(),
            controller_stability_summary: StabilitySummary {
                command_oscillation_detected: false,
                idle_output_stable: true,
                runtime_jitter_us: 5.0,
                missed_tick_count: 0,
                steady_state_reached: true,
            },
            verifier_status: "pass".into(),
            verifier_reason: None,
            controller_digest: "ctrl".into(),
            model_digest: "model".into(),
            calibration_digest: "cal".into(),
            frame_snapshot_id: 7,
            manifest_digest: "manifest".into(),
            wit_world_version: "bedrock:controller@1.0.0".into(),
            execution_mode: ExecutionMode::Live,
            compiler_version: "wasmtime".into(),
            created_at: Utc::now(),
            state_freshness: FreshnessState::Fresh,
        }
    }

    #[test]
    fn archive_round_trips_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let archive = EvidenceArchive::new(dir.path());
        let bundle = sample_bundle();

        let path = archive.save(&bundle).unwrap();
        assert!(path.exists());

        let loaded = archive.load(&bundle.bundle_id).unwrap();
        assert_eq!(loaded.bundle_id, bundle.bundle_id);
        assert_eq!(loaded.controller_id, bundle.controller_id);
        assert_eq!(loaded.execution_mode, bundle.execution_mode);
    }
}
