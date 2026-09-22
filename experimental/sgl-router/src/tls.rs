// SPDX-FileCopyrightText: Copyright (c) 2026 The SGLang Authors
// SPDX-License-Identifier: Apache-2.0

//! Outbound TLS provider and optional exclusive CA bundle.

use anyhow::{ensure, Context, Result};
use kube::client::ConfigExt;
use rustls::{crypto::CryptoProvider, ClientConfig, RootCertStore};
#[cfg(feature = "fips")]
use rustls_openssl::{cipher_suite, kx_group};
use rustls_pki_types::{pem::PemObject, CertificateDer};
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};

pub(crate) const FIPS: bool = cfg!(feature = "fips");

struct ProviderState {
    crypto: Arc<CryptoProvider>,
    #[cfg(feature = "fips")]
    _openssl: openssl::provider::Provider,
}

static PROVIDER: LazyLock<Result<ProviderState>> = LazyLock::new(|| {
    #[cfg(feature = "fips")]
    let openssl = openssl::provider::Provider::load(None, "fips")
        .context("load OpenSSL FIPS provider; check the runtime image and OPENSSL_CONF")?;
    let crypto = provider();
    require_fips_provider(&crypto)?;
    if crypto.clone().install_default().is_err() {
        ensure!(
            !FIPS,
            "a Rustls provider was installed before FIPS initialization"
        );
    }
    crypto
        .secure_random
        .fill(&mut [0; 32])
        .map_err(|_| anyhow::anyhow!("initialize TLS random generator"))?;
    Ok(ProviderState {
        crypto: Arc::new(crypto),
        #[cfg(feature = "fips")]
        _openssl: openssl,
    })
});

static TLS: OnceLock<Result<TlsSettings>> = OnceLock::new();

struct TlsSettings {
    http: ClientConfig,
    roots: Option<Vec<CertificateDer<'static>>>,
}

fn provider() -> CryptoProvider {
    #[cfg(feature = "fips")]
    {
        rustls_openssl::custom_provider(
            vec![
                cipher_suite::TLS13_AES_256_GCM_SHA384,
                cipher_suite::TLS13_AES_128_GCM_SHA256,
                cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
                cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
                cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
                cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            ],
            vec![kx_group::SECP384R1, kx_group::SECP256R1],
        )
    }
    #[cfg(not(feature = "fips"))]
    {
        rustls::crypto::ring::default_provider()
    }
}

fn require_fips_provider(provider: &CryptoProvider) -> Result<()> {
    ensure!(
        !FIPS || provider.fips(),
        "FIPS build requires a FIPS-enabled crypto provider; \
         OpenSSL deployments must configure fips=yes in OPENSSL_CONF"
    );
    Ok(())
}

impl TlsSettings {
    fn load(path: Option<&Path>) -> Result<Self> {
        let provider = PROVIDER
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;

        let roots = path
            .map(|path| {
                let certs = CertificateDer::pem_file_iter(path)
                    .with_context(|| format!("read CA bundle at {}", path.display()))?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .context("parse CA bundle certificates")?;
                ensure!(!certs.is_empty(), "CA bundle contains no certificates");
                Ok::<_, anyhow::Error>(certs)
            })
            .transpose()?;
        let mut store = RootCertStore::empty();
        match &roots {
            Some(certs) => {
                for cert in certs {
                    store.add(cert.clone()).context("invalid CA certificate")?;
                }
            }
            None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let mut http = ClientConfig::builder_with_provider(provider.crypto.clone())
            .with_safe_default_protocol_versions()
            .context("configure TLS protocol versions")?
            .with_root_certificates(store)
            .with_no_client_auth();
        http.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        http.require_ems = FIPS;
        ensure!(
            !FIPS || http.fips(),
            "FIPS build requires a FIPS-enabled TLS configuration"
        );
        Ok(Self { http, roots })
    }

    fn client_builder(&self) -> reqwest::ClientBuilder {
        reqwest::Client::builder().use_preconfigured_tls(self.http.clone())
    }

    fn kubernetes_client(&self, mut config: kube::Config) -> Result<kube::Client> {
        ensure!(
            !FIPS || !config.accept_invalid_certs,
            "FIPS build rejects Kubernetes insecure-skip-tls-verify"
        );
        if let Some(roots) = &self.roots {
            config.root_cert = Some(roots.iter().map(|cert| cert.to_vec()).collect());
        }
        ensure!(
            !FIPS || config.rustls_client_config()?.fips(),
            "Kubernetes TLS configuration is not in FIPS mode"
        );
        kube::Client::try_from(config).context("configure Kubernetes TLS client")
    }
}

fn settings() -> Result<&'static TlsSettings> {
    TLS.get_or_init(|| TlsSettings::load(None))
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error:#}"))
}

/// Initialize once, before tokenizers, discovery, or background tasks create clients.
pub fn initialize(ca_bundle: Option<&Path>) -> Result<()> {
    ensure!(
        TLS.set(TlsSettings::load(ca_bundle)).is_ok(),
        "outbound TLS is already initialized"
    );
    settings()?;
    #[cfg(feature = "fips")]
    tracing::info!(
        openssl_version = openssl::version::version(),
        "outbound TLS uses the runtime OpenSSL FIPS provider"
    );
    Ok(())
}

pub fn client_builder() -> Result<reqwest::ClientBuilder> {
    Ok(settings()?.client_builder())
}

pub(crate) fn kubernetes_client(config: kube::Config) -> Result<kube::Client> {
    settings()?.kubernetes_client(config)
}

#[cfg(test)]
mod tests {
    use super::{require_fips_provider, TlsSettings, FIPS};
    use axum::http::{Request, Response, Version};
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use kube::client::ConfigExt;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
    #[cfg(feature = "fips")]
    use rustls::{CipherSuite, NamedGroup};
    use rustls::{ServerConfig, SupportedProtocolVersion};
    use rustls_pki_types::PrivatePkcs8KeyDer;
    use std::{convert::Infallible, io::Write, sync::Arc, time::Duration};
    use tempfile::NamedTempFile;
    use tokio::{net::TcpListener, task::JoinHandle};
    use tokio_rustls::TlsAcceptor;

    struct Server {
        url: String,
        ca: NamedTempFile,
        task: JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[cfg(feature = "fips")]
    #[test]
    fn fips_algorithms_do_not_expand_with_runtime_provider_capabilities() {
        TlsSettings::load(None).unwrap();
        let provider = &super::PROVIDER.as_ref().unwrap().crypto;
        assert_eq!(
            provider
                .kx_groups
                .iter()
                .map(|group| group.name())
                .collect::<Vec<_>>(),
            [NamedGroup::secp384r1, NamedGroup::secp256r1]
        );
        assert!(provider.cipher_suites.iter().all(|suite| matches!(
            suite.suite(),
            CipherSuite::TLS13_AES_256_GCM_SHA384
                | CipherSuite::TLS13_AES_128_GCM_SHA256
                | CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
                | CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
                | CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
                | CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
        )));
    }

    async fn server(version: &'static SupportedProtocolVersion, h2: bool) -> Server {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .signed_by(&key, &ca, &ca_key)
            .unwrap();
        let mut ca_file = NamedTempFile::new().unwrap();
        ca_file.write_all(ca.pem().as_bytes()).unwrap();
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[version])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
                )
                .unwrap();
        config.alpn_protocols = vec![if h2 {
            b"h2".to_vec()
        } else {
            b"http/1.1".to_vec()
        }];
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let service =
                        service_fn(|request: Request<hyper::body::Incoming>| async move {
                            let body = if request.uri().path() == "/auth" {
                                format!(
                                    "{} {}",
                                    request.headers()["authorization"].to_str().unwrap(),
                                    request.headers()["x-test"].to_str().unwrap()
                                )
                            } else {
                                "ok".into()
                            };
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(body))))
                        });
                    if h2 {
                        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    } else {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    }
                });
            }
        });
        Server {
            url,
            ca: ca_file,
            task,
        }
    }

    #[tokio::test]
    async fn https_requires_trusted_ca_and_matching_hostname_with_both_protocols() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            for h2 in [false, true] {
                let server = server(version, h2).await;
                let trusted = TlsSettings::load(Some(server.ca.path())).unwrap();
                assert_eq!(trusted.http.fips(), FIPS);
                let client = trusted
                    .client_builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap();
                let response = client.get(&server.url).send().await.unwrap();
                assert_eq!(
                    response.version(),
                    if h2 {
                        Version::HTTP_2
                    } else {
                        Version::HTTP_11
                    }
                );
                assert_eq!(response.text().await.unwrap(), "ok");

                let wrong_host = server.url.replace("localhost", "127.0.0.1");
                let error = client.get(wrong_host).send().await.unwrap_err();
                assert!(format!("{error:?}").to_lowercase().contains("certificate"));

                let untrusted = TlsSettings::load(None)
                    .unwrap()
                    .client_builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap();
                let error = untrusted.get(&server.url).send().await.unwrap_err();
                assert!(format!("{error:?}").to_lowercase().contains("certificate"));
            }
        }
    }

    #[tokio::test]
    async fn kubernetes_preserves_cluster_ca_auth_and_server_name_override() {
        let server = server(&rustls::version::TLS12, false).await;
        let mut config = kube::Config::new(
            server
                .url
                .replace("localhost", "127.0.0.1")
                .parse()
                .unwrap(),
        );
        let roots = TlsSettings::load(Some(server.ca.path()))
            .unwrap()
            .roots
            .unwrap();
        config.root_cert = Some(roots.iter().map(|cert| cert.to_vec()).collect());
        config.tls_server_name = Some("localhost".into());
        config.auth_info.token = Some("test-token".into());
        config.headers = vec![("x-test".parse().unwrap(), "configured".parse().unwrap())];
        let settings = TlsSettings::load(None).unwrap();
        let tls = config.rustls_client_config().unwrap();
        assert_eq!(tls.require_ems, FIPS);
        assert_eq!(tls.fips(), FIPS);
        let client = settings.kubernetes_client(config.clone()).unwrap();
        let response = client
            .request_text(Request::get("/auth").body(Vec::new()).unwrap())
            .await
            .unwrap();
        assert_eq!(response, "Bearer test-token configured");

        config.tls_server_name = None;
        let client = settings.kubernetes_client(config).unwrap();
        assert!(client
            .request_text(Request::get("/").body(Vec::new()).unwrap())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn exclusive_bundle_applies_to_kubernetes_and_rejects_insecure_config() {
        let server = server(&rustls::version::TLS13, false).await;
        let settings = TlsSettings::load(Some(server.ca.path())).unwrap();
        let mut config = kube::Config::new(server.url.parse().unwrap());
        config.root_cert = Some(vec![]);
        let client = settings.kubernetes_client(config.clone()).unwrap();
        assert_eq!(
            client
                .request_text(Request::get("/").body(Vec::new()).unwrap())
                .await
                .unwrap(),
            "ok"
        );
        if FIPS {
            config.accept_invalid_certs = true;
            let error = settings.kubernetes_client(config).err().unwrap();
            assert!(error.to_string().contains("insecure-skip-tls-verify"));
            assert!(require_fips_provider(&rustls::crypto::ring::default_provider()).is_err());
        }
    }

    #[test]
    fn invalid_ca_bundle_never_falls_back_to_public_roots() {
        let mut file = NamedTempFile::new().unwrap();
        assert!(TlsSettings::load(Some(file.path())).is_err());
        file.write_all(b"-----BEGIN CERTIFICATE-----\ninvalid\n-----END CERTIFICATE-----\n")
            .unwrap();
        assert!(TlsSettings::load(Some(file.path())).is_err());
    }
}
