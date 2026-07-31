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

        if let Err(existing) = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default() {
            assert!(
                std::ptr::eq(existing, &jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER),
                "the fips feature is enabled but a non-FIPS jsonwebtoken crypto provider is already installed"
            );
        }
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
