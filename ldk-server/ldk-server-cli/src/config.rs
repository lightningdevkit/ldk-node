// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const DEFAULT_CONFIG_FILE: &str = "config.toml";
const DEFAULT_CERT_FILE: &str = "tls.crt";
const API_KEY_FILE: &str = "api_key";

pub fn get_default_data_dir() -> Option<PathBuf> {
	#[cfg(target_os = "macos")]
	{
		#[allow(deprecated)] // todo can remove once we update MSRV to 1.87+
		std::env::home_dir().map(|home| home.join("Library/Application Support/ldk-server"))
	}
	#[cfg(target_os = "windows")]
	{
		std::env::var("APPDATA").ok().map(|appdata| PathBuf::from(appdata).join("ldk-server"))
	}
	#[cfg(not(any(target_os = "macos", target_os = "windows")))]
	{
		#[allow(deprecated)] // todo can remove once we update MSRV to 1.87+
		std::env::home_dir().map(|home| home.join(".ldk-server"))
	}
}

pub fn get_default_config_path() -> Option<PathBuf> {
	get_default_data_dir().map(|dir| dir.join(DEFAULT_CONFIG_FILE))
}

pub fn get_default_cert_path() -> Option<PathBuf> {
	get_default_data_dir().map(|path| path.join(DEFAULT_CERT_FILE))
}

pub fn get_default_api_key_path(network: &str) -> Option<PathBuf> {
	get_default_data_dir().map(|path| path.join(network).join(API_KEY_FILE))
}

#[derive(Debug, Deserialize)]
pub struct Config {
	pub node: NodeConfig,
	pub tls: Option<TlsConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TlsConfig {
	pub cert_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NodeConfig {
	pub rest_service_address: String,
	network: String,
}

impl Config {
	pub fn network(&self) -> Result<String, String> {
		match self.node.network.as_str() {
			"bitcoin" | "mainnet" => Ok("bitcoin".to_string()),
			"testnet" => Ok("testnet".to_string()),
			"testnet4" => Ok("testnet4".to_string()),
			"signet" => Ok("signet".to_string()),
			"regtest" => Ok("regtest".to_string()),
			other => Err(format!("Unsupported network: {other}")),
		}
	}
}

pub fn load_config(path: &PathBuf) -> Result<Config, String> {
	let contents = std::fs::read_to_string(path)
		.map_err(|e| format!("Failed to read config file '{}': {}", path.display(), e))?;
	toml::from_str(&contents)
		.map_err(|e| format!("Failed to parse config file '{}': {}", path.display(), e))
}
