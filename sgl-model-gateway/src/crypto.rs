//! Process-wide rustls crypto provider selection.
//!
//! Default builds use the `ring` provider. Builds with the `fips` feature
//! use the aws-lc-rs FIPS provider (AWS-LC FIPS module) for all rustls-based
//! TLS (HTTPS server, Kubernetes client, outbound HTTP clients).

/// Installs the process-wide rustls crypto provider.
///
/// Safe to call multiple times; only the first call installs the provider.
pub fn ensure_crypto_provider_installed() {
    #[cfg(feature = "fips")]
    let provider = rustls::crypto::default_fips_provider();
    #[cfg(not(feature = "fips"))]
    let provider = rustls::crypto::ring::default_provider();

    let _ = provider.install_default();

    #[cfg(feature = "fips")]
    {
        let installed = rustls::crypto::CryptoProvider::get_default()
            .expect("a rustls crypto provider must be installed");
        assert!(
            installed.fips(),
            "the fips feature is enabled but the installed rustls crypto provider is not FIPS-validated"
        );

        // jsonwebtoken's process default can only be set once. Record whether
        // this crate's aws-lc install won that race; if some other code
        // installed a provider first, it may not be FIPS-validated.
        static JWT_AWS_LC_INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let installed_by_us = *JWT_AWS_LC_INSTALLED.get_or_init(|| {
            jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER
                .install_default()
                .is_ok()
        });
        assert!(
            installed_by_us,
            "the fips feature is enabled but a jsonwebtoken crypto provider was already installed by other code"
        );
    }
}

/// Incremental SHA-256 hasher backed by AWS-LC in `fips` builds and by
/// RustCrypto's `sha2` otherwise.
pub struct Sha256Hasher {
    #[cfg(feature = "fips")]
    inner: aws_lc_rs::digest::Context,
    #[cfg(not(feature = "fips"))]
    inner: sha2::Sha256,
}

impl Sha256Hasher {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "fips")]
            inner: aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256),
            #[cfg(not(feature = "fips"))]
            inner: <sha2::Sha256 as sha2::Digest>::new(),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        #[cfg(feature = "fips")]
        self.inner.update(data);
        #[cfg(not(feature = "fips"))]
        sha2::Digest::update(&mut self.inner, data);
    }

    pub fn finalize(self) -> [u8; 32] {
        #[cfg(feature = "fips")]
        {
            self.inner
                .finish()
                .as_ref()
                .try_into()
                .expect("SHA-256 digest is 32 bytes")
        }
        #[cfg(not(feature = "fips"))]
        {
            sha2::Digest::finalize(self.inner).into()
        }
    }
}

impl Default for Sha256Hasher {
    fn default() -> Self {
        Self::new()
    }
}
