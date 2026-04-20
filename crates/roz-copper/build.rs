fn main() {
    // Required by cu29-derive's `#[copper_runtime]` macro.
    // The macro writes log-index metadata to this directory at compile time.
    println!("cargo:rustc-env=LOG_INDEX_DIR={}", std::env::var("OUT_DIR").unwrap());

    // Compile v1 (substrate.sim) and v2 (substrate.sim.v2) proto files
    // side-by-side. Each file's `package` declaration produces a separate
    // Rust file under OUT_DIR:
    //   $OUT_DIR/substrate.sim.rs     (from proto/substrate/sim/bridge.proto)
    //   $OUT_DIR/substrate.sim.v2.rs  (from proto/substrate/sim/v2/bridge.proto)
    // Each is included via its own `tonic::include_proto!` call on the Rust
    // side (v1 in `io_grpc::proto`, v2 in `proto_v2` — see crates/roz-copper/src/lib.rs).
    // v1 stays wire-compatible per Phase 25 D-05; v2 is new per 25-CONTEXT.md.
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(
            &[
                "proto/substrate/sim/bridge.proto",
                "proto/substrate/sim/v2/bridge.proto",
            ],
            &["proto"],
        )
        .expect("failed to compile substrate.sim v1 + v2 protos");

    // Explicit rerun triggers so incremental builds pick up edits to either
    // proto file. tonic-build emits these automatically, but we're explicit
    // to match the workspace convention and catch v2 edits reliably.
    println!("cargo:rerun-if-changed=proto/substrate/sim/bridge.proto");
    println!("cargo:rerun-if-changed=proto/substrate/sim/v2/bridge.proto");
}
