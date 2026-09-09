//! Logging from the app: a `log` backend that hands records to the host,
//! which delivers them live to the embedder (`Phase::Log`). Installed by
//! [`crate::app!`]; use the `log` macros as usual.

use crate::bindings::terminal as t;

struct HostLogger;

impl log::Log for HostLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let level = match record.level() {
            log::Level::Trace => t::LogLevel::Trace,
            log::Level::Debug => t::LogLevel::Debug,
            log::Level::Info => t::LogLevel::Info,
            log::Level::Warn => t::LogLevel::Warn,
            log::Level::Error => t::LogLevel::Error,
        };
        t::log(level, record.target(), &record.args().to_string());
    }

    fn flush(&self) {}
}

static LOGGER: HostLogger = HostLogger;

/// Route the `log` facade to the host. Called once by the entry point;
/// harmless if called again.
pub fn install() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Trace);
    }
}
