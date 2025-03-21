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

impl TryFrom<TomlConfig> for Config {
	type Error = io::Error;

	fn try_from(toml_config: TomlConfig) -> io::Result<Self> {
		let listening_addr =
			SocketAddress::from_str(&toml_config.node.listening_address).map_err(|e| {
				io::Error::new(
					io::ErrorKind::InvalidInput,
					format!("Invalid listening address configured: {}", e),
				)
			})?;
		let rest_service_addr = SocketAddr::from_str(&toml_config.node.rest_service_address)
			.map_err(|e| {
				io::Error::new(
					io::ErrorKind::InvalidInput,
					format!("Invalid rest service address configured: {}", e),
				)
			})?;
		let bitcoind_rpc_addr =
			SocketAddr::from_str(&toml_config.bitcoind.rpc_address).map_err(|e| {
				io::Error::new(
					io::ErrorKind::InvalidInput,
					format!("Invalid bitcoind RPC address configured: {}", e),
				)
			})?;

		let (rabbitmq_connection_string, rabbitmq_exchange_name) = {
			let rabbitmq = toml_config.rabbitmq.unwrap_or(RabbitmqConfig {
				connection_string: String::new(),
				exchange_name: String::new(),
			});
			#[cfg(feature = "events-rabbitmq")]
			if rabbitmq.connection_string.is_empty() || rabbitmq.exchange_name.is_empty() {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"Both `rabbitmq.connection_string` and `rabbitmq.exchange_name` must be configured if enabling `events-rabbitmq` feature.".to_string(),
				));
			}
			(rabbitmq.connection_string, rabbitmq.exchange_name)
		};

		Ok(Config {
			listening_addr,
			network: toml_config.node.network,
			rest_service_addr,
			storage_dir_path: toml_config.storage.disk.dir_path,
			bitcoind_rpc_addr,
			bitcoind_rpc_user: toml_config.bitcoind.rpc_user,
			bitcoind_rpc_password: toml_config.bitcoind.rpc_password,
			rabbitmq_connection_string,
			rabbitmq_exchange_name,
		})
	}
}

/// Configuration loaded from a TOML file.
#[derive(Deserialize, Serialize)]
pub struct TomlConfig {
	node: NodeConfig,
	storage: StorageConfig,
	bitcoind: BitcoindConfig,
	rabbitmq: Option<RabbitmqConfig>,
}

#[derive(Deserialize, Serialize)]
struct NodeConfig {
	network: Network,
	listening_address: String,
	rest_service_address: String,
}

#[derive(Deserialize, Serialize)]
struct StorageConfig {
	disk: DiskConfig,
}

#[derive(Deserialize, Serialize)]
struct DiskConfig {
	dir_path: String,
}

#[derive(Deserialize, Serialize)]
struct BitcoindConfig {
	rpc_address: String,
	rpc_user: String,
	rpc_password: String,
}

#[derive(Deserialize, Serialize)]
struct RabbitmqConfig {
	connection_string: String,
	exchange_name: String,
}

/// Loads the configuration from a TOML file at the given path.
pub fn load_config<P: AsRef<Path>>(config_path: P) -> io::Result<Config> {
	let file_contents = fs::read_to_string(config_path.as_ref()).map_err(|e| {
		io::Error::new(
			e.kind(),
			format!("Failed to read config file '{}': {}", config_path.as_ref().display(), e),
		)
	})?;

	let toml_config: TomlConfig = toml::from_str(&file_contents).map_err(|e| {
		io::Error::new(
			io::ErrorKind::InvalidData,
			format!("Config file contains invalid TOML format: {}", e),
		)
	})?;
	Ok(Config::try_from(toml_config)?)
}

#[cfg(test)]
mod tests {
	use super::*;
	use ldk_node::{bitcoin::Network, lightning::ln::msgs::SocketAddress};
	use std::str::FromStr;

	#[test]
	fn test_read_toml_config_from_file() {
		let storage_path = std::env::temp_dir();
		let config_file_name = "config.toml";

		let toml_config = r#"
			[node]
			network = "regtest"
			listening_address = "localhost:3001"
			rest_service_address = "127.0.0.1:3002"
			
			[storage.disk]
			dir_path = "/tmp"
			
			[bitcoind]
			rpc_address = "127.0.0.1:8332"    # RPC endpoint
			rpc_user = "bitcoind-testuser"
			rpc_password = "bitcoind-testpassword"
			
			[rabbitmq]
			connection_string = "rabbitmq_connection_string"
			exchange_name = "rabbitmq_exchange_name"
			"#;

		fs::write(storage_path.join(config_file_name), toml_config).unwrap();

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
