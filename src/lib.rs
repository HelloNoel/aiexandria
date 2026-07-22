//! AIexandria — give LLMs hierarchical, sandboxed access to git-hosted docs.
//!
//! Public surface is minimal: a [`server::AIexandriaServer`] backed by a
//! [`registry::RegistryConfig`] and a [`cache::RepoCache`].

pub mod auth;
pub mod cache;
pub mod command;
pub mod error;
pub mod generate;
pub mod index;
pub mod registry;
pub mod resolver;
pub mod server;

pub use error::{AIexError, Result};

/// Default config directory: `$XDG_CONFIG_HOME/aiexandria` if set, else
/// `~/.config/aiexandria`. We don't use `dirs::config_dir()` because on
/// macOS it returns `~/Library/Application Support`, which conflicts with
/// the Unix-style paths AIexandria's docs and examples reference.
pub fn config_dir() -> Option<std::path::PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(std::path::PathBuf::from(xdg).join("aiexandria"));
        }
    }
    dirs::home_dir().map(|h| h.join(".config").join("aiexandria"))
}
