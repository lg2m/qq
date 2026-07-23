//! Terminal user interface and client-side state.

#![forbid(unsafe_code)]

mod app;
mod client;
mod settings;
mod terminal;
mod view;

pub use app::{TuiError, TuiOptions, run};
pub use client::{ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState};
pub use settings::{Action, KeyChord, Layout, Settings, SettingsBuilder, SettingsError};
