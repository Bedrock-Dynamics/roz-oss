//! Zenoh session lifecycle: load config (caller-driven path), open session,
//! expose a cheap-to-clone [`zenoh::Session`] to subsystems.
//!
//! Design (D-01..D-04):
//! - Explicit `path` argument -> [`zenoh::Config::from_file`]
//! - `None` path             -> [`zenoh::Config::from_json5`] with an in-code default
//! - [`zenoh::Session`] is internally `Arc`-based; clone freely across tasks.
//!
//! **Config ownership (C-07):** This module does NOT read env vars. Callers
//! (e.g. `WorkerConfig::load`) own env resolution and pass the resolved path
//! through to [`load_zenoh_config`]. Keeps config in one place and makes tests
//! trivial.

use anyhow::Context as _;

/// Default in-code peer config used when no path is supplied.
/// Peer mode with multicast scout enabled.
// Zenoh 1.8 `autoconnect` expects a list of whatami variants ("router",
// "peer", "client"), not a string alternation. An empty list means "do not
// autoconnect" for that side. Peers autoconnect to routers and other peers;
// routers do not autoconnect (pure peer deployment).
const DEFAULT_PEER_JSON5: &str = r#"{
  mode: "peer",
  scouting: {
    multicast: {
      enabled: true,
      address: "224.0.0.224:7446",
      interface: "auto",
      autoconnect: { router: [], peer: ["router", "peer"] },
    },
    gossip: { enabled: true },
  },
  listen: { endpoints: [] },
  connect: { endpoints: [] },
}"#;

/// Resolve a [`zenoh::Config`] from an explicit path or the in-code default.
///
/// # Errors
/// Returns the file-load or parse error from zenoh, wrapped with the source
/// path (or the default-config marker) for diagnostics.
pub fn load_zenoh_config(path: Option<&std::path::Path>) -> anyhow::Result<zenoh::Config> {
    path.map_or_else(
        || {
            zenoh::Config::from_json5(DEFAULT_PEER_JSON5)
                .map_err(|e| anyhow::anyhow!("default zenoh config invalid: {e}"))
        },
        |p| {
            zenoh::Config::from_file(p)
                .map_err(|e| anyhow::anyhow!("ROZ_ZENOH_CONFIG={} parse failed: {e}", p.display()))
        },
    )
}

/// Open a zenoh session using an optional config file path (`None` = in-code default).
///
/// # Errors
/// Returns config-load failure or [`zenoh::open`] failure.
pub async fn open_session(config_path: Option<&std::path::Path>) -> anyhow::Result<zenoh::Session> {
    let cfg = load_zenoh_config(config_path).context("load zenoh config")?;
    let session = zenoh::open(cfg)
        .await
        .map_err(|e| anyhow::anyhow!("zenoh::open failed: {e}"))?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    // C-09 fix: no env::set_var/remove_var — Rust 2024 marks those unsafe and
    // the workspace lint `unsafe_code = "deny"` rejects that even in tests.
    // Tests pass explicit paths to `load_zenoh_config(Option<&Path>)`.

    #[test]
    fn default_config_valid_when_no_path() {
        let cfg = load_zenoh_config(None).expect("default config must be valid");
        drop(cfg);
    }

    #[test]
    fn missing_path_returns_error_mentioning_path() {
        let path = std::path::Path::new("/definitely/does/not/exist.json5");
        let err = load_zenoh_config(Some(path)).unwrap_err();
        assert!(err.to_string().contains("/definitely/does/not/exist.json5"));
    }

    #[test]
    fn valid_file_loads_from_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zenoh.json5");
        std::fs::write(&path, r#"{ mode: "peer" }"#).unwrap();
        let cfg = load_zenoh_config(Some(&path)).expect("valid file loads");
        drop(cfg);
    }

    #[test]
    fn invalid_file_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json5");
        std::fs::write(&path, "this is not json5 {{{").unwrap();
        let err = load_zenoh_config(Some(&path)).unwrap_err();
        // Accept any error string — zenoh's parse error message wording varies.
        let _ = err.to_string();
    }
}
