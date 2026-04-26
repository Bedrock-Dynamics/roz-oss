//! Verify-only PX4 fixture recording hygiene contract.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const ENV_NAME: &str = "ROZ_PX4_FIXTURE_RECORD_VERIFY";
const FIXTURE_ROOT: &str = "crates/roz-mavlink/tests/fixtures";
const READINESS_FIXTURES: &[&str] = &["readiness/px4/takeoff.json", "readiness/px4/land.json"];

#[test]
#[ignore = "env-gated PX4 fixture recording verify-only; set ROZ_PX4_FIXTURE_RECORD_VERIFY=1"]
fn px4_fixture_record_verify_writes_only_tempdir_and_compares_checked_in() -> anyhow::Result<()> {
    if env::var(ENV_NAME).as_deref() != Ok("1") {
        println!("ROZ_PX4_FIXTURE_RECORD_VERIFY not set; skipping verify-only PX4 fixture recording");
        return Ok(());
    }

    let before = fixture_git_status()?;
    let tempdir = tempfile::tempdir()?;
    record_px4_fixtures_to_dir(tempdir.path())?;
    compare_recorded_to_checked_in(tempdir.path())?;
    let after = fixture_git_status()?;

    assert_eq!(
        before, after,
        "PX4 fixture recording must not mutate checked-in fixtures"
    );
    Ok(())
}

fn compare_recorded_to_checked_in(output_dir: &Path) -> anyhow::Result<()> {
    for relative_path in READINESS_FIXTURES {
        let checked_in = fs::read(Path::new(FIXTURE_ROOT).join(relative_path))?;
        let recorded = fs::read(output_dir.join(relative_path))?;
        anyhow::ensure!(
            recorded == checked_in,
            "fixture drift requires a reviewed PR: {relative_path}"
        );
    }
    Ok(())
}

fn fixture_git_status() -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(["status", "--short", "--", FIXTURE_ROOT])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git status for MAVLink fixtures failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

mod tempfile {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    pub fn tempdir() -> io::Result<TempDir> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!("roz-px4-fixture-record-{}-{unique}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(TempDir { path })
    }
}
