//! Generated bindings for the `rattery:tui/app` world: the terminal and
//! websocket imports, and the async `run` export that [`crate::app!`] wires up.

wit_bindgen::generate!({
    path: "wit",
    world: "app",
    pub_export_macro: true,
    default_bindings_module: "rattery_app::bindings",
    // Import parameters borrow: a frame's cells reference the buffer's
    // symbols instead of allocating a String per cell.
    ownership: Borrowing { duplicate_if_necessary: false },
    additional_derives: [Clone, PartialEq, Eq],
});

pub use self::rattery::tui::{terminal, websocket};
