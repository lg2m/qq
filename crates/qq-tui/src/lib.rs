//! Terminal user interface and client-side state.

#![forbid(unsafe_code)]

mod app;
#[cfg(feature = "bench-support")]
pub mod bench_support;
mod composer;
mod settings;
mod terminal;
mod view;

pub use app::{ModelOption, TuiError, TuiOptions, run};
pub use qq_client::{ClientFailure, ClientPort, ClientRequest, ClientUpdate, ConnectionState};
pub use settings::{Action, KeyChord, Layout, Settings, SettingsBuilder, SettingsError};
