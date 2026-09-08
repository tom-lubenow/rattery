//! `rattery`: run a ratatui app packaged as a WASI 0.2 component inside the
//! current terminal, with the same isolation a browser gives a web page.

mod bindings;
mod convert;
mod http;
mod loader;
mod state;
mod terminal;

use anyhow::{Context, Result, bail};
use clap::Parser;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Cache, Config, Engine, Store};
use wasmtime_wasi::p2::bindings::Command;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{I32Exit, WasiCtxBuilder};

use crate::state::HostState;

/// Run a ratatui app delivered as a WASI component, sandboxed like a web page.
#[derive(Debug, Parser)]
#[command(name = "rattery", version)]
struct Cli {
    /// URL (http/https) or local path of the app component (.wasm).
    source: String,

    /// Origin the app's server functions are sent to. Defaults to the origin
    /// of SOURCE when it is a URL; required for server calls from a local file.
    #[arg(long, value_name = "URL")]
    origin: Option<String>,

    /// Extra origins the app may reach over HTTP (repeatable).
    #[arg(long = "allow-origin", value_name = "URL")]
    allow_origins: Vec<String>,

    /// Let the app reach any origin over HTTP.
    #[arg(long)]
    allow_all_origins: bool,

    /// Do not report mouse events to the app.
    #[arg(long)]
    no_mouse: bool,

    /// Skip the on-disk cache of compiled components.
    #[arg(long)]
    no_cache: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let loaded = loader::load(&cli.source, cli.origin.as_deref()).await?;
    let policy = http::OriginPolicy::new(
        loaded.origin.as_deref(),
        &cli.allow_origins,
        cli.allow_all_origins,
    )?;

    let mut config = Config::new();
    config.epoch_interruption(true);
    if !cli.no_cache {
        let cache = Cache::from_file(None)
            .map_err(anyhow::Error::from)
            .context("failed to configure the compile cache")?;
        config.cache(Some(cache));
    }
    let engine = Engine::new(&config)?;

    // Compile before touching the terminal so errors print normally.
    let component = Component::new(&engine, &loaded.bytes)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("{} is not a valid component", loaded.description))?;

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    bindings::terminal::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;

    // The guest gets no filesystem, no sockets, no inherited stdio: only the
    // terminal interface, the clock, randomness, and HTTP to allowed origins.
    let stdout = MemoryOutputPipe::new(1 << 20);
    let stderr = MemoryOutputPipe::new(1 << 20);
    let mut wasi = WasiCtxBuilder::new();
    wasi.stdout(stdout.clone())
        .stderr(stderr.clone())
        .arg("app");
    if let Some(origin) = &loaded.origin {
        wasi.env("RATTERY_ORIGIN", origin);
    }
    let wasi = wasi.build();

    let session = terminal::Session::enter(!cli.no_mouse)?;
    let (term, kill_switch) = terminal::TerminalHost::start(loaded.origin.clone());

    // Escape hatch for an unresponsive app: Ctrl-C three times in a row
    // interrupts the guest through wasmtime's epoch mechanism.
    let killed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    tokio::spawn({
        let engine = engine.clone();
        let killed = killed.clone();
        async move {
            if kill_switch.await.is_ok() {
                killed.store(true, std::sync::atomic::Ordering::SeqCst);
                engine.increment_epoch();
            }
        }
    });

    let mut store = Store::new(&engine, HostState::new(wasi, policy, term));
    store.set_epoch_deadline(1);

    let outcome = async {
        let command = Command::instantiate_async(&mut store, &component, &linker).await?;
        command.wasi_cli_run().call_run(&mut store).await
    }
    .await;

    drop(store);
    drop(session);

    let guest_stderr = stderr.contents();
    if !guest_stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&guest_stderr));
    }
    let guest_stdout = stdout.contents();
    if !guest_stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&guest_stdout));
    }

    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(())) => bail!("app exited with an error"),
        Err(err) => {
            if let Some(exit) = err.downcast_ref::<I32Exit>() {
                if exit.0 == 0 {
                    return Ok(());
                }
                bail!("app exited with status {}", exit.0);
            }
            if killed.load(std::sync::atomic::Ordering::SeqCst) {
                bail!("app terminated by rattery (Ctrl-C pressed three times)");
            }
            Err(anyhow::Error::from(err).context("app trapped"))
        }
    }
}
