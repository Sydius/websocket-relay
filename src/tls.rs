use anyhow::{Context, Result, anyhow};
use rustls_pemfile::{certs, private_key};
use std::{fs::File, io::BufReader};
use tokio_rustls::rustls;

use crate::config::TlsConfig;

pub fn load_tls_config(tls_config: &TlsConfig) -> Result<rustls::ServerConfig> {
    let cert_file = File::open(&tls_config.cert_file)
        .with_context(|| format!("Failed to open certificate file: {}", tls_config.cert_file))?;
    let key_file = File::open(&tls_config.key_file)
        .with_context(|| format!("Failed to open private key file: {}", tls_config.key_file))?;

    let cert_chain = certs(&mut BufReader::new(cert_file))
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse certificate file")?;

    if cert_chain.is_empty() {
        return Err(anyhow!("No certificates found in certificate file"));
    }

    let private_key = private_key(&mut BufReader::new(key_file))
        .context("Failed to parse private key file")?
        .ok_or_else(|| anyhow!("No private key found in key file"))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .context("Failed to create TLS server config")?;

    Ok(config)
}
