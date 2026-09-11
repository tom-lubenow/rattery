//! Build a rattery app inside `build.rs`, so the shim that embeds it is one
//! `cargo build` away.
//!
//! ```no_run
//! // build.rs of the shim
//! rattery_build::App::new("../app").build();
//! ```
//!
//! ```ignore
//! // main.rs of the shim
//! let report = rattery::App::from_bytes(rattery::embed!().to_vec())
//!     .origin("https://api.example.com")
//!     .run_blocking()?;
//! ```
//!
//! [`App::build`] compiles the app crate for `wasm32-wasip2` (release by
//! default) into a target directory under `OUT_DIR`, copies the component to
//! `OUT_DIR`, and exports its path as the `RATTERY_APP_WASM` environment
//! variable for `include_bytes!` (`rattery::embed!` is that one line).
//! Several apps can be built; [`App::env`] picks each one's variable.
//! Changes under the app crate's `src` or to its `Cargo.toml` trigger a
//! rebuild.
//!
//! With the `precompile` feature, [`App::precompile`] also compiles the
//! component to native code for the shim's target and exports the result as
//! `RATTERY_APP_CWASM`, so the shim starts without compiling anything:
//!
//! ```ignore
//! // build.rs
//! rattery_build::App::new("../app").precompile(true).build();
//! // main.rs: the bytes were produced by this build, so they can be trusted.
//! let app = unsafe { rattery::App::from_precompiled(rattery::embed_precompiled!().to_vec()) };
//! ```
//!
//! The nested build's target directory lives under `OUT_DIR`, which CI
//! caches usually skip; set `RATTERY_BUILD_TARGET_DIR` to a cached path
//! (say `target/rattery-build` of the shim) to keep the wasm dependency
//! build between runs.
//!
//! The nested build needs the `wasm32-wasip2` target installed
//! (`rustup target add wasm32-wasip2`).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// One app crate to compile.
#[derive(Debug, Clone)]
pub struct App {
    path: PathBuf,
    package: Option<String>,
    release: bool,
    features: Vec<String>,
    env_name: String,
    precompile: bool,
}

impl App {
    /// The app crate, by path relative to the shim's `Cargo.toml` (or absolute).
    pub fn new(path: impl AsRef<Path>) -> Self {
        let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self {
            path: manifest_dir.join(path.as_ref()),
            package: None,
            release: true,
            features: Vec::new(),
            env_name: "RATTERY_APP_WASM".into(),
            precompile: false,
        }
    }

    /// Also compile the component to native code for the shim's `TARGET`
    /// and export its path as the `_CWASM` variant of the variable
    /// (`RATTERY_APP_CWASM` by default). Needs the `precompile` feature.
    #[cfg(feature = "precompile")]
    pub fn precompile(mut self, yes: bool) -> Self {
        self.precompile = yes;
        self
    }

    /// The package to build when `path` is a workspace (`cargo build -p`).
    pub fn package(mut self, name: impl Into<String>) -> Self {
        self.package = Some(name.into());
        self
    }

    /// Build in the dev profile instead of release.
    pub fn debug(mut self, yes: bool) -> Self {
        self.release = !yes;
        self
    }

    /// Enable a cargo feature of the app crate.
    pub fn feature(mut self, name: impl Into<String>) -> Self {
        self.features.push(name.into());
        self
    }

    /// The environment variable that will hold the component's path
    /// (default `RATTERY_APP_WASM`). Use distinct names for several apps.
    pub fn env(mut self, name: impl Into<String>) -> Self {
        self.env_name = name.into();
        self
    }

    /// Compile the app and export its path. Panics with a readable message on
    /// failure, which is what a build script wants.
    pub fn build(self) -> PathBuf {
        match self.try_build() {
            Ok(path) => path,
            Err(err) => panic!("rattery-build: {err}"),
        }
    }

    /// [`App::build`] without the panic.
    pub fn try_build(self) -> Result<PathBuf, String> {
        let out_dir = PathBuf::from(
            env::var_os("OUT_DIR").ok_or("OUT_DIR is not set; call this from build.rs")?,
        );
        let manifest = self.path.join("Cargo.toml");
        if !manifest.exists() {
            return Err(format!("no Cargo.toml at {}", manifest.display()));
        }
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rerun-if-changed={}", self.path.join("src").display());
        println!(
            "cargo:rerun-if-changed={}",
            self.path.join("Cargo.lock").display()
        );

        // A target directory of its own: the outer cargo holds the lock on
        // the shim's, and the app is built for another target anyway. Under
        // OUT_DIR by default; RATTERY_BUILD_TARGET_DIR moves it somewhere a
        // CI cache can keep.
        println!("cargo:rerun-if-env-changed=RATTERY_BUILD_TARGET_DIR");
        let target_dir = env::var_os("RATTERY_BUILD_TARGET_DIR")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| out_dir.join("rattery-target"));
        let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.arg("build")
            .arg("--target")
            .arg("wasm32-wasip2")
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target_dir);
        if self.release {
            cmd.arg("--release");
        }
        if let Some(package) = &self.package {
            cmd.arg("-p").arg(package);
        }
        if !self.features.is_empty() {
            cmd.arg("--features").arg(self.features.join(","));
        }
        // Do not let the outer build's settings leak into the nested one.
        for key in [
            "CARGO_ENCODED_RUSTFLAGS",
            "RUSTFLAGS",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET",
            "CARGO_MAKEFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
        ] {
            cmd.env_remove(key);
        }
        let status = cmd
            .status()
            .map_err(|e| format!("failed to run cargo: {e}"))?;
        if !status.success() {
            return Err(format!(
                "building {} for wasm32-wasip2 failed (is the target installed? `rustup target add wasm32-wasip2`)",
                manifest.display()
            ));
        }

        let profile = if self.release { "release" } else { "debug" };
        let artifacts = target_dir.join("wasm32-wasip2").join(profile);
        let name = match &self.package {
            Some(package) => package.clone(),
            None => package_name(&manifest)?,
        };
        let candidates = [
            artifacts.join(format!("{name}.wasm")),
            artifacts.join(format!("{}.wasm", name.replace('-', "_"))),
        ];
        let built = candidates
            .iter()
            .find(|p| p.exists())
            .ok_or_else(|| format!("no component found at {}", candidates[0].display()))?;
        let dest = out_dir.join(format!("{name}.wasm"));
        std::fs::copy(built, &dest)
            .map_err(|e| format!("failed to copy {}: {e}", built.display()))?;
        println!("cargo:rustc-env={}={}", self.env_name, dest.display());
        if self.precompile {
            self.precompile_to(&dest)?;
        }
        Ok(dest)
    }

    #[cfg(feature = "precompile")]
    fn precompile_to(&self, component: &Path) -> Result<(), String> {
        let target =
            env::var("TARGET").map_err(|_| "TARGET is not set; call this from build.rs")?;
        let bytes = std::fs::read(component)
            .map_err(|e| format!("failed to read {}: {e}", component.display()))?;
        let native = rattery::precompile(&bytes, Some(&target))
            .map_err(|e| format!("precompiling for {target} failed: {e:#}"))?;
        let dest = component.with_extension("cwasm");
        std::fs::write(&dest, native)
            .map_err(|e| format!("failed to write {}: {e}", dest.display()))?;
        let env_name = match self.env_name.strip_suffix("_WASM") {
            Some(stem) => format!("{stem}_CWASM"),
            None => format!("{}_CWASM", self.env_name),
        };
        println!("cargo:rustc-env={env_name}={}", dest.display());
        Ok(())
    }

    #[cfg(not(feature = "precompile"))]
    fn precompile_to(&self, _component: &Path) -> Result<(), String> {
        Err("precompile needs the `precompile` feature of rattery-build".into())
    }
}

/// The `[package] name` of a manifest, without pulling in a TOML parser.
fn package_name(manifest: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(manifest)
        .map_err(|e| format!("failed to read {}: {e}", manifest.display()))?;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "name"
        {
            return Ok(value.trim().trim_matches('"').to_owned());
        }
    }
    Err(format!("no [package] name in {}", manifest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_package_name() {
        let dir = std::env::temp_dir().join(format!("rattery-build-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("Cargo.toml");
        std::fs::write(&manifest, "[workspace]\nmembers = []\n\n[package]\nname = \"my-app\"\nversion = \"0.1.0\"\n\n[dependencies]\nname = \"not this\"\n").unwrap();
        assert_eq!(package_name(&manifest).unwrap(), "my-app");
        let _ = std::fs::remove_dir_all(dir);
    }
}
