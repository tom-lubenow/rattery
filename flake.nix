{
  description = "rattery - a sandboxed terminal host for ratatui apps delivered over HTTP";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Stable Rust with the WASI preview 2 target for guest components.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
          targets = [ "wasm32-wasip2" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            wasm-tools
            wasmtime
            cargo-nextest
            pkg-config
            openssl
            gh
          ];

          RUST_BACKTRACE = "1";

          shellHook = ''
            echo "rattery dev shell"
            echo "Rust: $(rustc --version)"
          '';
        };
      }
    );
}
