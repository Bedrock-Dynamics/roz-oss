fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .btree_map([".roz.v1"])
        .compile_protos(
            &[
                "../../proto/roz/v1/tasks.proto",
                "../../proto/roz/v1/agent.proto",
                "../../proto/roz/v1/embodiment.proto",
                "../../proto/roz/v1/skills.proto",
                "../../proto/roz/v1/observability.proto", // Phase 26 OBS-03
                // Phase 26.9 D-10 — Foxglove schemas consumed by `commands::mcap`
                // RRD export for decoding `/tf`, `/roz/telemetry/pose`, `/roz/log`
                // MCAP payloads. The `FrameTransform`, `PoseInFrame`, and `Log`
                // messages declare `package foxglove;` so tonic-build emits them
                // under the `foxglove` module name. Transitive deps (`Pose`,
                // `Quaternion`, `Vector3`) are included so prost-build can
                // resolve imports.
                "../../proto/foxglove/FrameTransform.proto",
                "../../proto/foxglove/PoseInFrame.proto",
                "../../proto/foxglove/Log.proto",
                "../../proto/foxglove/Pose.proto",
                "../../proto/foxglove/Quaternion.proto",
                "../../proto/foxglove/Vector3.proto",
            ],
            &["../../proto"],
        )?;
    Ok(())
}
