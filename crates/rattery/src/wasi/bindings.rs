//! Generated bindings for the `rattery:tui/terminal` interface.

wit_bindgen::generate!({
    path: "../../wit",
    world: "app",
    with: {
        "wasi:io/poll@0.2.12": wasip2::io::poll,
    },
    additional_derives: [Clone, PartialEq, Eq],
});

pub use self::rattery::tui::terminal;
