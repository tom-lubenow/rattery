//! Generated bindings for the `rattery:tui/app` world: the terminal and
//! websocket imports, and the async `run` export that [`crate::app!`] wires up.

wit_bindgen::generate!({
    path: "../../wit",
    world: "app",
    pub_export_macro: true,
    default_bindings_module: "rattery::bindings",
    additional_derives: [Clone, PartialEq, Eq],
});

pub use self::rattery::tui::{terminal, websocket};
