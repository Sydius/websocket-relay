use anyhow::{Context, Result};
use serde::Deserialize;
use std::{collections::HashMap, fs};

#[derive(Deserialize)]
pub struct Config {
    pub listen: ListenConfig,
    pub targets: HashMap<String, TargetConfig>,
}

#[derive(Deserialize)]
pub struct ListenConfig {
    pub ip: String,
    pub port: u16,
    pub allowed_proxy_ips: Option<Vec<String>>,
    pub tls: Option<TlsConfig>,
}

#[derive(Deserialize)]
pub struct TlsConfig {
    pub cert_file: String,
    pub key_file: String,
}

#[derive(Clone, Deserialize)]
pub struct TargetConfig {
    pub host: String,
    pub port: u16,
}

pub fn load_config() -> Result<Config> {
    let content = fs::read_to_string("config.toml").context("Failed to read config.toml file")?;
    toml::from_str(&content).context("Failed to parse config.toml as valid TOML")
}
