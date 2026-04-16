//! Phase 18 PLAN-10 Task 3: live-server E2E smoke test for `roz skill`.
//!
//! Marked `#[ignore]` per RESEARCH §Validation Architecture — exercises the
//! full CLI → tonic gRPC → SkillsServiceImpl pipeline against a running
//! roz-server. Listed by `cargo test -- --list` so CI compilation gates the
//! shape, even though the live invocation is opt-in.
//!
//! ```bash
//! # Bring up a roz-server (separate terminal):
//! cargo run -p roz-server --bin roz-server
//!
//! # Then:
//! ROZ_API_URL=http://127.0.0.1:8080 ROZ_API_KEY=... \
//!   cargo test -p roz-cli --test skill_e2e_smoke -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::process::Command;

fn cargo_bin(name: &str) -> PathBuf {
    if let Ok(path) = std::env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(path);
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target");
    path.push("debug");
    path.push(name);
    path
}

fn roz() -> Command {
    Command::new(cargo_bin("roz"))
}

#[test]
#[ignore = "live-server e2e; run with --ignored"]
fn e2e_skill_import_show_export_roundtrip() {
    // Per CONTEXT D-04 / D-05: import takes a directory path that the CLI
    // tars + streams over gRPC.
    let fixture_dir = workspace_root().join("examples/test-skill");
    assert!(fixture_dir.exists(), "fixture dir missing: {}", fixture_dir.display());

    // import
    let out = roz()
        .args(["skill", "import", &fixture_dir.to_string_lossy()])
        .output()
        .expect("spawn import");
    assert!(
        out.status.success(),
        "import failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // list — must include "test-skill"
    let out = roz().args(["skill", "list"]).output().expect("spawn list");
    assert!(out.status.success(), "list failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("test-skill"),
        "list output missing test-skill: {stdout}"
    );

    // show
    let out = roz()
        .args(["skill", "show", "test-skill"])
        .output()
        .expect("spawn show");
    assert!(out.status.success(), "show failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# Test Skill") || stdout.contains("test-skill"));

    // export — write to a tempfile, assert non-zero size
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let out_path = tmp.path().with_extension("tar.gz");
    let out = roz()
        .args(["skill", "export", "test-skill", "--out", &out_path.to_string_lossy()])
        .output()
        .expect("spawn export");
    assert!(out.status.success(), "export failed");
    let meta = std::fs::metadata(&out_path).expect("export file exists");
    assert!(meta.len() > 0, "export file is empty");

    // delete
    let out = roz()
        .args(["skill", "delete", "test-skill"])
        .output()
        .expect("spawn delete");
    assert!(out.status.success(), "delete failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // versions_deleted should report ≥1 in the human-readable output.
    assert!(
        stdout.contains("1") || stdout.contains("deleted"),
        "delete output missing version count: {stdout}"
    );
}

fn workspace_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path
}
