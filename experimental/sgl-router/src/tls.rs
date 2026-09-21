// SPDX-FileCopyrightText: Copyright (c) 2026 The SGLang Authors
// SPDX-License-Identifier: Apache-2.0

//! Outbound TLS provider and optional exclusive CA bundle.

use anyhow::{ensure, Context, Result};
use rustls::{crypto::CryptoProvider, ClientConfig, RootCertStore};
use rustls_pki_types::{pem::PemObject, CertificateDer};
use std::path::Path;
use std::sync::{Arc, LazyLock, OnceLock};

#[cfg(all(feature = "fips-aws-lc", feature = "fips-openssl"))]
compile_error!("select only one FIPS backend: fips-openssl (or fips) or fips-aws-lc");

pub(crate) const FIPS: bool = cfg!(any(feature = "fips-aws-lc", feature = "fips-openssl"));

struct ProviderState {
    crypto: Arc<CryptoProvider>,
    #[cfg(feature = "fips-openssl")]
    _openssl: openssl::provider::Provider,
}

static PROVIDER: LazyLock<Result<ProviderState>> = LazyLock::new(|| {
    #[cfg(feature = "fips-openssl")]
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
        #[cfg(feature = "fips-openssl")]
        _openssl: openssl,
    })
});

static TLS: OnceLock<Result<TlsSettings>> = OnceLock::new();

struct TlsSettings {
    http: ClientConfig,
    roots: Option<Vec<CertificateDer<'static>>>,
}

fn provider() -> CryptoProvider {
    #[cfg(feature = "fips-openssl")]
    {
        rustls_openssl::default_provider()
    }
    #[cfg(all(feature = "fips-aws-lc", not(feature = "fips-openssl")))]
    {
        rustls::crypto::aws_lc_rs::default_provider()
    }
    #[cfg(not(any(feature = "fips-aws-lc", feature = "fips-openssl")))]
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
        #[cfg(feature = "fips-openssl")]
        {
            kubernetes_openssl::client(config)
        }
        #[cfg(not(feature = "fips-openssl"))]
        {
            kube::Client::try_from(config).context("configure Kubernetes TLS client")
        }
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
    #[cfg(feature = "fips-aws-lc")]
    tracing::info!(
        aws_lc_version = aws_lc_rs::awslc_version(),
        fips_module = ?aws_lc_rs::fips_version(),
        "outbound TLS uses AWS-LC in FIPS mode"
    );
    #[cfg(feature = "fips-openssl")]
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

#[cfg(feature = "fips-openssl")]
mod kubernetes_openssl {
    use anyhow::{ensure, Context, Result};
    use hyper_rustls::{FixedServerNameResolver, HttpsConnectorBuilder};
    use hyper_timeout::TimeoutConnector;
    use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioExecutor};
    use kube::client::{Body, ConfigExt};
    use tower::ServiceBuilder;
    use tower_http::trace::TraceLayer;

    pub(super) fn client(config: kube::Config) -> Result<kube::Client> {
        let mut tls = config.rustls_client_config()?;
        tls.require_ems = true;
        ensure!(
            tls.fips(),
            "Kubernetes TLS configuration is not in FIPS mode"
        );
        let mut https = HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http();
        if let Some(name) = &config.tls_server_name {
            https = https.with_server_name_resolver(FixedServerNameResolver::new(
                name.clone()
                    .try_into()
                    .context("invalid Kubernetes TLS server name")?,
            ));
        }
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        let mut connector = TimeoutConnector::new(https.enable_http1().wrap_connector(http));
        connector.set_connect_timeout(config.connect_timeout);
        connector.set_read_timeout(config.read_timeout);
        connector.set_write_timeout(config.write_timeout);
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
            .build::<_, Body>(connector);
        let service = ServiceBuilder::new()
            .layer(config.base_uri_layer())
            .option_layer(config.auth_layer()?)
            .layer(config.extra_headers_layer()?)
            .layer(TraceLayer::new_for_http())
            .map_err(tower::BoxError::from)
            .service(client);
        Ok(kube::Client::new(service, config.default_namespace))
    }
}

#[cfg(test)]
mod tests {
    use super::{require_fips_provider, TlsSettings, FIPS};
    use axum::http::{Request, Response, Version};
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
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
