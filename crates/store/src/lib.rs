//! The browser's persistent store (plan D02, D03): one database per
//! profile, in Turso's engine (a Rust rewrite of SQLite: its SQL and file
//! format), encrypted as a whole. The key is random and kept beside the
//! database until a passphrase can be set (D03). History is its first
//! table; cookies, localStorage, passwords and bookmarks join it later.
//!
//! The profile lives in `%LOCALAPPDATA%\browser\Profile` on Windows,
//! `~/Library/Application Support/browser/Profile` on macOS and
//! `$XDG_DATA_HOME/browser/Profile` (else `~/.local/share/...`) elsewhere.

#![forbid(unsafe_code)]

mod db;
mod history;

pub use db::Store;
pub use history::{HistoryEntry, HistoryService};

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store file: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(String),
    #[error("no profile folder: {0} is not set")]
    NoProfile(&'static str),
    #[error("the key file is damaged")]
    BadKey,
}

/// The profile folder for this platform.
pub fn default_profile_dir() -> Result<PathBuf, StoreError> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or(StoreError::NoProfile("LOCALAPPDATA"))?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME").ok_or(StoreError::NoProfile("HOME"))?)
            .join("Library")
            .join("Application Support")
    } else {
        match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            Some(d) => PathBuf::from(d),
            None => PathBuf::from(std::env::var_os("HOME").ok_or(StoreError::NoProfile("HOME"))?)
                .join(".local")
                .join("share"),
        }
    };
    Ok(base.join("browser").join("Profile"))
}
