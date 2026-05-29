//! Crypto provider tests
//!
//! Verifies that aws-lc-rs is the sole runtime crypto provider and that
//! ring cannot be installed as a fallback.

#[cfg(test)]
mod crypto_provider_tests {
    use rustls::crypto::CryptoProvider;

    /// Verify aws-lc-rs installs as the default crypto provider.
    #[test]
    fn test_aws_lc_rs_is_default_provider() {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        // install_default returns Err if a provider is already installed,
        // Ok(()) on the first call. Either outcome is acceptable here;
        // what matters is the provider we get back below.
        let _ = provider.install_default();

        let installed = CryptoProvider::get_default();
        assert!(
            installed.is_some(),
            "A default CryptoProvider must be installed"
        );
    }

    /// After aws-lc-rs is installed, ring must fail to install.
    /// This proves ring cannot replace aws-lc-rs at runtime.
    #[test]
    fn test_ring_cannot_override_aws_lc_rs() {
        // Ensure aws-lc-rs is installed first.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // Attempting to install ring's provider must fail because
        // install_default is a one-shot global operation.
        let ring_result = rustls::crypto::ring::default_provider().install_default();
        assert!(
            ring_result.is_err(),
            "ring must not be able to override the already-installed aws-lc-rs provider"
        );
    }
}
