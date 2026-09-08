fn main() {
    // Compiles ../app for wasm32-wasip2 and exports RATTERY_APP_WASM.
    rattery_build::App::new("../app").build();
}
