//! Process-wide rustls crypto provider selection.
//!
//! The provider is chosen at compile time via cargo features:
//! - `tls-ring` (default): the ring backend.
//! - `fips`: the aws-lc-rs FIPS backend (CMVP-validated AWS-LC
//!   cryptographic module), restricted to FIPS-approved algorithms.

use rustls::crypto::CryptoProvider;

#[cfg(feature = "fips")]
fn provider() -> CryptoProvider {
    rustls::crypto::default_fips_provider()
}

#[cfg(all(not(feature = "fips"), feature = "tls-ring"))]
fn provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

#[cfg(all(not(feature = "fips"), not(feature = "tls-ring")))]
compile_error!("either the `tls-ring` or `fips` feature must be enabled");

/// Install the selected crypto provider as the process-wide default.
/// Safe to call multiple times; subsequent calls are no-ops.
pub fn install_default_provider() {
    let _ = provider().install_default();
}

/// Whether the active default provider is in FIPS mode.
pub fn fips_active() -> bool {
    CryptoProvider::get_default()
        .map(|p| p.fips())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent() {
        install_default_provider();
        install_default_provider();
        assert!(CryptoProvider::get_default().is_some());
    }

    #[cfg(feature = "fips")]
    #[test]
    fn fips_provider_is_active() {
        install_default_provider();
        assert!(fips_active());
    }
}
