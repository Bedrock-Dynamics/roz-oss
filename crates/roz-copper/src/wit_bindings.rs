pub mod live_controller {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "live-controller",
    });
}
