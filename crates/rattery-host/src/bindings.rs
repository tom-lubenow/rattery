//! Host-side bindings for the `rattery:tui/app` world.

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "app",
    with: {
        "wasi": wasmtime_wasi::p2::bindings,
    },
    imports: { default: async | trappable },
    additional_derives: [Clone, PartialEq, Eq],
});

pub use self::rattery::tui::terminal;
