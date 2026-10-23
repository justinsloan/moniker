//! GTK-free core of Moniker. Must never depend on GTK or VTE.

pub mod askpass;
pub mod bundle;
pub mod crypto;
pub mod ping;
pub mod profile;
pub mod secrets;
pub mod ssh;
pub mod ssh_config;
pub mod store;
pub mod sync;
pub mod transcript;

/// Crate version, as set in the workspace manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
        assert!(
            VERSION.split('.').count() >= 3,
            "expected a semver-style version, got {VERSION}"
        );
    }
}
