fn main() {
    // Compiles ../app for wasm32-wasip2 (RATTERY_APP_WASM), then to native
    // code for this target (RATTERY_APP_CWASM) so the shim starts instantly.
    rattery_build::App::new("../app").precompile(true).build();
}
