use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::{fs, io};

/// Configuration for LDK Server.
#[derive(PartialEq, Eq, Debug)]
pub struct Config {
	pub listening_addr: SocketAddress,
	pub network: Network,
	pub rest_service_addr: SocketAddr,
	pub storage_dir_path: String,
	pub bitcoind_rpc_addr: SocketAddr,
	pub bitcoind_rpc_user: String,
	pub bitcoind_rpc_password: String,
	pub rabbitmq_connection_string: String,
	pub rabbitmq_exchange_name: String,
}

impl TryFrom<JsonConfig> for Config {
	type Error = io::Error;

	fn try_from(json_config: JsonConfig) -> io::Result<Self> {
		let listening_addr =
			SocketAddress::from_str(&json_config.listening_address).map_err(|e| {
				io::Error::new(
					io::ErrorKind::InvalidInput,
					format!("Invalid listening address configured: {}", e),
				)
			})?;
		let rest_service_addr =
			SocketAddr::from_str(&json_config.rest_service_address).map_err(|e| {
				io::Error::new(
					io::ErrorKind::InvalidInput,
					format!("Invalid rest service address configured: {}", e),
				)
			})?;

		let bitcoind_rpc_addr =
			SocketAddr::from_str(&json_config.bitcoind_rpc_address).map_err(|e| {
				io::Error::new(
					io::ErrorKind::InvalidInput,
					format!("Invalid bitcoind RPC address configured: {}", e),
				)
			})?;

		#[cfg(feature = "events-rabbitmq")]
		if json_config.rabbitmq_connection_string.as_deref().map_or(true, |s| s.is_empty())
			|| json_config.rabbitmq_exchange_name.as_deref().map_or(true, |s| s.is_empty())
		{
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"Both `rabbitmq_connection_string` and `rabbitmq_exchange_name` must be configured if enabling `events-rabbitmq` feature.".to_string(),
			));
		}

		Ok(Config {
			listening_addr,
			network: json_config.network,
			rest_service_addr,
			storage_dir_path: json_config.storage_dir_path,
			bitcoind_rpc_addr,
			bitcoind_rpc_user: json_config.bitcoind_rpc_user,
			bitcoind_rpc_password: json_config.bitcoind_rpc_password,
			rabbitmq_connection_string: json_config.rabbitmq_connection_string.unwrap_or_default(),
			rabbitmq_exchange_name: json_config.rabbitmq_exchange_name.unwrap_or_default(),
		})
	}
}

/// Configuration loaded from a JSON file.
#[derive(Deserialize, Serialize)]
pub struct JsonConfig {
	listening_address: String,
	network: Network,
	rest_service_address: String,
	storage_dir_path: String,
	bitcoind_rpc_address: String,
	bitcoind_rpc_user: String,
	bitcoind_rpc_password: String,
	rabbitmq_connection_string: Option<String>,
	rabbitmq_exchange_name: Option<String>,
}

/// Loads the configuration from a JSON file at the given path.
pub fn load_config<P: AsRef<Path>>(config_path: P) -> io::Result<Config> {
	let file_contents = fs::read_to_string(config_path.as_ref()).map_err(|e| {
		io::Error::new(
			e.kind(),
			format!("Failed to read config file '{}': {}", config_path.as_ref().display(), e),
		)
	})?;

	let json_string = remove_json_comments(file_contents.as_str());
	let json_config: JsonConfig = serde_json::from_str(&json_string).map_err(|e| {
		io::Error::new(
			io::ErrorKind::InvalidData,
			format!("Config file contains invalid JSON format: {}", e),
		)
	})?;
	Ok(Config::try_from(json_config)?)
}

fn remove_json_comments(s: &str) -> String {
	s.lines()
		.map(|line| if let Some(pos) = line.find("//") { &line[..pos] } else { line })
		.collect::<Vec<&str>>()
		.join("\n")
}

#[cfg(test)]
mod tests {
	use super::*;
	use ldk_node::{bitcoin::Network, lightning::ln::msgs::SocketAddress};
	use std::str::FromStr;

	#[test]
	fn test_read_json_config_from_file() {
		let storage_path = std::env::temp_dir();
		let config_file_name = "config.json";

		let json_config = r#"{
			"listening_address": "localhost:3001",
			"network": "regtest",
			"rest_service_address": "127.0.0.1:3002",
			"storage_dir_path": "/tmp",
			"bitcoind_rpc_address":"127.0.0.1:8332", // comment-1
			"bitcoind_rpc_user": "bitcoind-testuser",
			"bitcoind_rpc_password": "bitcoind-testpassword",
			"rabbitmq_connection_string": "rabbitmq_connection_string",
			"rabbitmq_exchange_name": "rabbitmq_exchange_name",
			"unknown_key": "random-value"
			// comment-2
			}"#;

		fs::write(storage_path.join(config_file_name), json_config).unwrap();

		assert_eq!(
			load_config(storage_path.join(config_file_name)).unwrap(),
			Config {
				listening_addr: SocketAddress::from_str("localhost:3001").unwrap(),
				network: Network::Regtest,
				rest_service_addr: SocketAddr::from_str("127.0.0.1:3002").unwrap(),
				storage_dir_path: "/tmp".to_string(),
				bitcoind_rpc_addr: SocketAddr::from_str("127.0.0.1:8332").unwrap(),
				bitcoind_rpc_user: "bitcoind-testuser".to_string(),
				bitcoind_rpc_password: "bitcoind-testpassword".to_string(),
				rabbitmq_connection_string: "rabbitmq_connection_string".to_string(),
				rabbitmq_exchange_name: "rabbitmq_exchange_name".to_string(),
			}
		)
	}
}
