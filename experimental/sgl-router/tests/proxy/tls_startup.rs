// SPDX-FileCopyrightText: Copyright (c) 2026 The SGLang Authors
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;
use tokio::process::Command;

fn router() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sgl-router"));
    command
        .args([
            "--model-id",
            "tiny",
            "--tokenizer-path",
            "tests/fixtures/tiny_tokenizer.json",
            "--worker-urls",
            "http://127.0.0.1:1",
            "--port",
            "0",
        ])
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn invalid_ca_bundle_stops_the_binary_before_serving() {
    let empty_bundle = tempfile::NamedTempFile::new().unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        router()
            .arg("--tls-ca-bundle")
            .arg(empty_bundle.path())
            .output(),
    )
    .await
    .expect("router must stop during TLS initialization")
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("initialize outbound TLS"), "{stderr}");
    assert!(stderr.contains("contains no certificates"), "{stderr}");
}

#[cfg(feature = "fips-openssl")]
#[tokio::test]
async fn openssl_without_fips_properties_stops_the_binary() {
    let config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        config.path(),
        include_str!("../fixtures/openssl-fips.cnf").replace(
            "default_properties = fips=yes",
            "default_properties = fips=no",
        ),
    )
    .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        router().env("OPENSSL_CONF", config.path()).output(),
    )
    .await
    .expect("router must require FIPS properties")
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("FIPS-enabled crypto provider"), "{stderr}");
}

#[tokio::test]
async fn https_indexer_endpoint_stops_the_binary_before_plaintext_transport() {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        router()
            .args([
                "--policy",
                "cache_aware",
                "--cache-prefix-provider",
                "indexer",
                "--kv-indexer-endpoint",
                "https://127.0.0.1:1",
            ])
            .output(),
    )
    .await
    .expect("router must reject unsupported indexer TLS")
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("KV Indexer transport has no TLS support"),
        "{stderr}"
    );
}

#[cfg(feature = "fips-openssl")]
#[tokio::test]
async fn missing_openssl_module_stops_the_binary_without_a_crypto_fallback() {
    let empty = tempfile::tempdir().unwrap();
    let config = tempfile::NamedTempFile::new().unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        router()
            .env("OPENSSL_MODULES", empty.path())
            .env("OPENSSL_CONF", config.path())
            .output(),
    )
    .await
    .expect("router must stop when the module is missing")
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("load OpenSSL FIPS provider"), "{stderr}");
}
