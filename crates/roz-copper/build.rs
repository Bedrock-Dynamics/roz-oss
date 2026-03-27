fn main() {
    // Required by cu29-derive's `#[copper_runtime]` macro.
    // The macro writes log-index metadata to this directory at compile time.
    println!("cargo:rustc-env=LOG_INDEX_DIR={}", std::env::var("OUT_DIR").unwrap());

    // Compile gRPC client stubs for Gazebo bridge integration tests.
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/substrate/sim/bridge.proto"], &["proto"])
        .expect("failed to compile bridge.proto");
}
