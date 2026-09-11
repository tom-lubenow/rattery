//! Updates, the way a page handles them: the host tells the app when a
//! newer version is pending ([`Event::UpdateChanged`](crate::event::Event::UpdateChanged)),
//! the app reads the state with [`pending`], saves what matters to
//! [`storage`](crate::storage), and calls [`reload`] when convenient.
//! [`check`] asks the host to look now, for apps that learn of updates
//! through their own server.

use std::time::Duration;

use crate::bindings::rattery::tui::terminal as t;

/// A newer version the host holds, validated and ready to run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Update {
    /// The server's validator for it (an ETag or a date). Opaque; show it
    /// or ignore it.
    pub version: Option<String>,
    /// Time left before the host may reload on its own, at the next idle
    /// moment. `None`: the host leaves it to the app.
    pub reload_after: Option<Duration>,
    /// Time left before the host reloads regardless of activity.
    pub reload_by: Option<Duration>,
}

impl From<t::Update> for Update {
    fn from(u: t::Update) -> Self {
        Self {
            version: u.version,
            reload_after: u.reload_after_ms.map(Duration::from_millis),
            reload_by: u.reload_by_ms.map(Duration::from_millis),
        }
    }
}

/// Whether this app can be updated at all, and by whom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Availability {
    /// The host checks on its own and tells the app.
    Watched,
    /// The host checks when the app calls [`check`].
    OnRequest,
    /// The host has nowhere to look (the component is embedded in it), so
    /// [`check`] finds nothing. The host's embedder may still hand it a
    /// version, which arrives as `UpdateChanged` like any other.
    Unavailable,
}

/// Whether this app can be updated at all, and by whom.
pub fn availability() -> Availability {
    match t::update_availability() {
        t::Availability::Watched => Availability::Watched,
        t::Availability::OnRequest => Availability::OnRequest,
        t::Availability::Unavailable => Availability::Unavailable,
    }
}

/// Ask the host to look for a newer version now, and return the pending
/// update afterwards (which may be one found earlier). Fails when the
/// source cannot be reached or refuses this host.
pub async fn check() -> Result<Option<Update>, String> {
    t::check_update().await.map(|u| u.map(Update::from))
}

/// The pending update, with its deadlines as of now.
pub fn pending() -> Option<Update> {
    t::pending_update().map(Update::from)
}

/// Restart the app in place on the pending update, or on the current
/// version if there is none. Never returns: the host tears this instance
/// down and starts a new one, so save anything that matters to
/// [`storage`](crate::storage) first.
pub fn reload() -> ! {
    t::reload();
    unreachable!("the host reinstantiates the app on reload")
}
