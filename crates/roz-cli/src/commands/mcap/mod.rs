//! Phase 26.9 — `roz mcap` command namespace.
//!
//! Currently exposes one subcommand: `to-rrd`, which converts session MCAP
//! files into Rerun `.rrd` recording files for substrate ingestion. Pure
//! format converter — writes to disk via `RecordingStream::save(path)`,
//! never spawns or connects to a viewer (CONTEXT D-01, D-04, SC4).
//!
//! Namespace is reserved for future sister commands (`info`, `validate`,
//! `repair`) per D-01; all other commands are deferred (CONTEXT `<deferred>`).
//!
//! The heavy lifting lives in the `export-rrd` feature-gated submodules.
//! Default `roz-cli` builds expose the subcommand surface but return a
//! friendly "rebuild with --features export-rrd" error when invoked (D-17).

use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::config::CliConfig;

// Phase 26.9 D-10 — Foxglove proto types (FrameTransform, PoseInFrame, Log,
// plus transitive Pose/Quaternion/Vector3) are compiled by `build.rs` into
// the `foxglove` package. Mounted here so Plan 05/06 emit paths can
// `prost::Message::decode` them. Feature-gated because they are only needed
// by the RRD export path; default builds do not pay the extra linker cost.
#[cfg(feature = "export-rrd")]
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    unused_qualifications,
    dead_code,
    reason = "generated prost code — upstream Foxglove proto shapes, no hand-editing; types consumed by Plans 05/06"
)]
pub(crate) mod foxglove {
    tonic::include_proto!("foxglove");
}

// Submodule declarations. Each submodule is feature-gated itself via
// `#![cfg(feature = "export-rrd")]` at the top of its file; these `mod`
// lines are unconditional so that rustc sees the file existence, but the
// content compiles to nothing when the feature is off.
pub(crate) mod camera;
pub(crate) mod export;
pub(crate) mod recording;
pub(crate) mod text_logs;
pub(crate) mod transforms;

/// `roz mcap` subcommand namespace.
#[derive(Debug, Args)]
pub struct McapArgs {
    #[command(subcommand)]
    pub command: McapCommands,
}

#[derive(Debug, Subcommand)]
pub enum McapCommands {
    /// Convert one or more MCAP session files to Rerun .rrd recording files
    /// for substrate ingestion (Phase 26.9).
    ToRrd(ToRrdArgs),
}

/// Arguments for `roz mcap to-rrd` (CONTEXT D-02 / D-03).
///
/// Two mutually exclusive modes:
///
/// - **Single:** `<INPUT_MCAP> --output <OUTPUT_RRD>` — fail-fast (D-04).
/// - **Bulk:** `--bulk <GLOB> --output-dir <DIR>` — continue-on-error (D-05);
///   output filename = `<input_basename>.rrd`.
///
/// The `conflicts_with` / `required_unless_present` combination enforces
/// mode exclusivity at clap parse time.
#[derive(Debug, Args)]
pub struct ToRrdArgs {
    /// Single-mode: input MCAP file. Conflicts with `--bulk`.
    #[arg(value_name = "INPUT_MCAP", conflicts_with = "bulk", required_unless_present = "bulk")]
    pub input: Option<PathBuf>,

    /// Single-mode: output .rrd path. Conflicts with `--output-dir`.
    #[arg(short, long, conflicts_with = "output_dir", required_unless_present = "bulk")]
    pub output: Option<PathBuf>,

    /// Bulk-mode: glob pattern (expanded by the binary, not the shell).
    /// Conflicts with positional `<INPUT_MCAP>`.
    #[arg(long, conflicts_with = "input")]
    pub bulk: Option<String>,

    /// Bulk-mode: output directory. Each input produces `<basename>.rrd`
    /// here. Conflicts with `--output`; requires `--bulk`.
    #[arg(long, conflicts_with = "output", requires = "bulk")]
    pub output_dir: Option<PathBuf>,
}

/// Dispatch entry called from `main.rs`.
pub async fn execute(cmd: &McapCommands, _config: &CliConfig) -> anyhow::Result<()> {
    match cmd {
        McapCommands::ToRrd(args) => to_rrd(args).await,
    }
}

#[cfg(not(feature = "export-rrd"))]
#[expect(
    clippy::unused_async,
    reason = "Plan 02 skeleton: signature mirrors the feature-on path so Plans 03-07 can land async I/O without re-shaping the dispatcher"
)]
async fn to_rrd(_args: &ToRrdArgs) -> anyhow::Result<()> {
    // D-17 — friendly error when the binary was built without the feature.
    anyhow::bail!(
        "this binary was built without --features=export-rrd; \
         rebuild with cargo install --features export-rrd"
    )
}

#[cfg(feature = "export-rrd")]
#[expect(
    clippy::unused_async,
    reason = "Plan 03: dispatcher routes to sync export entry points; Plans 04/07 may introduce real `.await` (e.g. tokio file I/O for camera frames) and remove this expect"
)]
async fn to_rrd(args: &ToRrdArgs) -> anyhow::Result<()> {
    if args.input.is_some() {
        // Single mode (D-04 fail-fast).
        let input = args.input.as_ref().expect("clap enforces presence");
        let output = args
            .output
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--output is required in single-file mode"))?;
        export::export_one(input, output)?;
        Ok(())
    } else {
        // Bulk mode (D-05 continue-on-error).
        let pattern = args.bulk.as_ref().expect("clap enforces presence");
        let out_dir = args
            .output_dir
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--output-dir is required in bulk mode"))?;
        export::export_bulk(pattern, out_dir)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Commands};

    #[test]
    fn parse_mcap_to_rrd_single() {
        let cli = Cli::parse_from(["roz", "mcap", "to-rrd", "/tmp/in.mcap", "--output", "/tmp/out.rrd"]);
        let Some(Commands::Mcap(args)) = cli.command else {
            panic!("expected Mcap command");
        };
        let super::McapCommands::ToRrd(to_rrd_args) = args.command;
        assert_eq!(
            to_rrd_args.input.as_ref().map(|p| p.as_path()),
            Some(std::path::Path::new("/tmp/in.mcap"))
        );
        assert_eq!(
            to_rrd_args.output.as_ref().map(|p| p.as_path()),
            Some(std::path::Path::new("/tmp/out.rrd"))
        );
        assert!(to_rrd_args.bulk.is_none());
        assert!(to_rrd_args.output_dir.is_none());
    }

    #[test]
    fn parse_mcap_to_rrd_bulk() {
        let cli = Cli::parse_from([
            "roz",
            "mcap",
            "to-rrd",
            "--bulk",
            "sessions/*.mcap",
            "--output-dir",
            "/tmp/out",
        ]);
        let Some(Commands::Mcap(args)) = cli.command else {
            panic!("expected Mcap command");
        };
        let super::McapCommands::ToRrd(to_rrd_args) = args.command;
        assert!(to_rrd_args.input.is_none());
        assert!(to_rrd_args.output.is_none());
        assert_eq!(to_rrd_args.bulk.as_deref(), Some("sessions/*.mcap"));
        assert_eq!(
            to_rrd_args.output_dir.as_ref().map(|p| p.as_path()),
            Some(std::path::Path::new("/tmp/out"))
        );
    }

    #[test]
    fn parse_mcap_to_rrd_rejects_mixed_modes() {
        // Positional + --bulk is invalid per D-03.
        let err = Cli::try_parse_from(["roz", "mcap", "to-rrd", "/tmp/in.mcap", "--bulk", "x/*.mcap"])
            .expect_err("mixed single + bulk modes must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot be used with") || msg.contains("conflicts_with"),
            "expected conflict error, got: {msg}"
        );
    }

    #[test]
    fn parse_mcap_to_rrd_rejects_missing_output_in_single_mode() {
        // Single-mode positional but no --output is invalid.
        let err = Cli::try_parse_from(["roz", "mcap", "to-rrd", "/tmp/in.mcap"])
            .expect_err("missing --output must fail in single mode");
        assert!(
            err.to_string().contains("--output") || err.to_string().contains("required"),
            "expected required-arg error, got: {err}"
        );
    }
}
