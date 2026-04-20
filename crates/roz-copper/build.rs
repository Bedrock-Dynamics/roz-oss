fn main() {
    // Required by cu29-derive's `#[copper_runtime]` macro.
    // The macro writes log-index metadata to this directory at compile time.
    println!("cargo:rustc-env=LOG_INDEX_DIR={}", std::env::var("OUT_DIR").unwrap());

    // Compile v1 (substrate.sim) and v2 (substrate.sim.v2) proto files in
    // TWO separate tonic-build invocations. Each file's `package` declaration
    // produces a separate Rust file under OUT_DIR:
    //   $OUT_DIR/substrate.sim.rs     (from proto/substrate/sim/bridge.proto)
    //   $OUT_DIR/substrate.sim.v2.rs  (from proto/substrate/sim/v2/bridge.proto)
    // Each is included via its own `tonic::include_proto!` call on the Rust
    // side (v1 in `io_grpc::proto`, v2 in `proto_v2` — see
    // crates/roz-copper/src/lib.rs).
    //
    // v1 stays wire-compatible per Phase 25 D-05; v2 is new per 25-CONTEXT.md.
    //
    // Two-invocation rationale (Rule 3 deviation vs. plan's single-invocation
    // form): v2 imports v1 primitives (Transform3D, Vector3, Quaternion,
    // JointCommandMode) per 25-03 Open Question #6 resolution. Because v2's
    // package `substrate.sim.v2` is a *child* of `substrate.sim`, prost-build
    // emits `super::Transform3D` by default — which, inside
    // `pub mod proto_v2 { tonic::include_proto!(...) }`, resolves to
    // `crate::Transform3D` (the lib.rs top level), NOT
    // `crate::io_grpc::proto::Transform3D`. The fix is per-type
    // `extern_path(".substrate.sim.Transform3D", "crate::io_grpc::proto::Transform3D")`
    // (and the other three primitives) on the v2 invocation only. Prost skips
    // regenerating those v1 types and rewrites v2's cross-package references
    // to the existing v1 module. Per-type (not `.substrate.sim`-wide) extern
    // paths are required so v2's own `substrate.sim.v2.*` types don't also
    // match the prefix and get externalized into nonexistence.
    //
    // Invocation ORDER matters: v2 runs FIRST because it writes a PARTIAL
    // `substrate.sim.rs` as a side-effect of its `import` processing (the
    // four extern_path'd types are skipped). v1 runs SECOND and OVERWRITES
    // that partial file with the full v1 generated code. v1's invocation
    // only writes `substrate.sim.rs`, never touching `substrate.sim.v2.rs`.
    // If we ran v1 first, v2's later invocation would clobber v1's file.
    //
    // A single-invocation form cannot express this per-type remap — the
    // plan's original rationale (single invocation is cheaper) was wrong
    // for the cross-package-import case the post-review 25-03 locked in.
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .extern_path(".substrate.sim.Transform3D", "crate::io_grpc::proto::Transform3D")
        .extern_path(".substrate.sim.Vector3", "crate::io_grpc::proto::Vector3")
        .extern_path(".substrate.sim.Quaternion", "crate::io_grpc::proto::Quaternion")
        .extern_path(
            ".substrate.sim.JointCommandMode",
            "crate::io_grpc::proto::JointCommandMode",
        )
        .compile_protos(&["proto/substrate/sim/v2/bridge.proto"], &["proto"])
        .expect("failed to compile substrate.sim.v2 proto");

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/substrate/sim/bridge.proto"], &["proto"])
        .expect("failed to compile substrate.sim v1 proto");

    // Explicit rerun triggers so incremental builds pick up edits to either
    // proto file. tonic-build emits these automatically, but we're explicit
    // to match the workspace convention and catch v2 edits reliably.
    println!("cargo:rerun-if-changed=proto/substrate/sim/bridge.proto");
    println!("cargo:rerun-if-changed=proto/substrate/sim/v2/bridge.proto");
}
