//! Key-value storage scoped to the app's origin, like a browser's
//! `localStorage`. The host keeps it private to the origin, bounds it by a
//! quota, and persists it between runs when it has an origin to key it by.
//! Values are bytes; [`set_string`] and [`get_string`] cover the common case.

use std::fmt;

use crate::bindings::rattery::tui::storage as s;

/// Why a write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The origin's quota (bytes or entries) would be exceeded.
    QuotaExceeded,
    /// The key or value is larger than allowed.
    TooLarge,
    /// The host has storage disabled.
    Disabled,
}

impl From<s::StorageError> for Error {
    fn from(e: s::StorageError) -> Self {
        match e {
            s::StorageError::QuotaExceeded => Error::QuotaExceeded,
            s::StorageError::TooLarge => Error::TooLarge,
            s::StorageError::Disabled => Error::Disabled,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::QuotaExceeded => write!(f, "storage quota exceeded"),
            Error::TooLarge => write!(f, "storage key or value too large"),
            Error::Disabled => write!(f, "storage is disabled by the host"),
        }
    }
}

impl std::error::Error for Error {}

pub fn get(key: &str) -> Option<Vec<u8>> {
    s::get(key)
}

pub fn set(key: &str, value: &[u8]) -> Result<(), Error> {
    s::set(key, value).map_err(Error::from)
}

pub fn remove(key: &str) {
    s::remove(key)
}

pub fn keys() -> Vec<String> {
    s::keys()
}

pub fn clear() {
    s::clear()
}

/// Bytes in use and the quota.
pub fn usage() -> (u64, u64) {
    s::usage()
}

pub fn get_string(key: &str) -> Option<String> {
    get(key).and_then(|bytes| String::from_utf8(bytes).ok())
}

pub fn set_string(key: &str, value: &str) -> Result<(), Error> {
    set(key, value.as_bytes())
}
