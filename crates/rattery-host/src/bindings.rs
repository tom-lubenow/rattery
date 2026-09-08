//! Host-side bindings for the `rattery:tui/app` world.

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "app",
    with: {
        "rattery:tui/websocket.socket": crate::websocket::WsSocket,
    },
    imports: { default: async | trappable },
    exports: { default: async | store },
    additional_derives: [Clone, PartialEq, Eq],
});

pub use self::rattery::tui::{terminal, websocket};
