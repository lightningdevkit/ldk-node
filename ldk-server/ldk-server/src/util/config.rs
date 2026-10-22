// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use std::net::SocketAddr;
use std::path::Path;
use std::str::FromStr;
use std::{fs, io};

use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::NodeAlias;
use ldk_node::liquidity::LSPS2ServiceConfig;
use log::LevelFilter;
use serde::{Deserialize, Serialize};

/// Configuration for LDK Server.
#[derive(Debug)]
pub struct Config {
	pub listening_addrs: Option<Vec<SocketAddress>>,
	pub announcement_addrs: Option<Vec<SocketAddress>>,
	pub alias: Option<NodeAlias>,
	pub network: Network,
	pub tls_config: Option<TlsConfig>,
	pub rest_service_addr: SocketAddr,
	pub storage_dir_path: Option<String>,
	pub chain_source: ChainSource,
	pub rabbitmq_connection_string: String,
	pub rabbitmq_exchange_name: String,
	pub lsps2_service_config: Option<LSPS2ServiceConfig>,
	pub log_level: LevelFilter,
	pub log_file_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
	pub cert_path: Option<String>,
	pub key_path: Option<String>,
	pub hosts: Vec<String>,
}

#[derive(Debug)]
pub enum ChainSource {
	Rpc { rpc_address: SocketAddr, rpc_user: String, rpc_password: String },
	Electrum { server_url: String },
	Esplora { server_url: String },
}

impl TryFrom<TomlConfig> for Config {
	type Error = io::Error;

	fn try_from(toml_config: TomlConfig) -> io::Result<Self> {
		let listening_addrs = toml_config
			.node
			.listening_addresses
			.map(|addresses| {
				addresses
					.into_iter()
					.map(|addr| {
						SocketAddress::from_str(&addr).map_err(|e| {
							io::Error::new(
								io::ErrorKind::InvalidInput,
								format!("Invalid listening address configured: {}", e),
							)
						})
					})
					.collect()
			})
			.transpose()?;
		let announcement_addrs = toml_config
			.node
			.announcement_addresses
			.map(|addresses| {
				addresses
					.into_iter()
					.map(|addr| {
						SocketAddress::from_str(&addr).map_err(|e| {
							io::Error::new(
								io::ErrorKind::InvalidInput,
								format!("Invalid announcement address configured: {}", e),
							)
						})
					})
					.collect()
			})
			.transpose()?;
		let rest_service_addr = SocketAddr::from_str(&toml_config.node.rest_service_address)
			.map_err(|e| {
				io::Error::new(
					io::ErrorKind::InvalidInput,
					format!("Invalid rest service address configured: {}", e),
				)
			})?;
		let chain_source = match (toml_config.esplora, toml_config.electrum, toml_config.bitcoind) {
			(Some(EsploraConfig { server_url }), None, None) => ChainSource::Esplora { server_url },
			(None, Some(ElectrumConfig { server_url }), None) => {
				ChainSource::Electrum { server_url }
			},
			(None, None, Some(BitcoindConfig { rpc_address, rpc_user, rpc_password })) => {
				let rpc_address = SocketAddr::from_str(&rpc_address).map_err(|e| {
					io::Error::new(
						io::ErrorKind::InvalidInput,
						format!("Invalid bitcoind RPC address configured: {}", e),
					)
				})?;
				ChainSource::Rpc { rpc_address, rpc_user, rpc_password }
			},
			(None, None, None) => {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"At least one chain source must be set, either esplora, electrum, or bitcoind"
						.to_string(),
				))
			},
			_ => {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"Must set a single chain source, multiple were configured".to_string(),
				))
			},
		};

		let alias = if let Some(alias_str) = toml_config.node.alias {
			let mut bytes = [0u8; 32];
			let alias_bytes = alias_str.trim().as_bytes();
			if alias_bytes.len() > 32 {
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"node.alias must be at most 32 bytes long.".to_string(),
				));
			}
			bytes[..alias_bytes.len()].copy_from_slice(alias_bytes);
			Some(NodeAlias(bytes))
		} else {
			None
		};

		let log_level = toml_config
			.log
			.as_ref()
			.and_then(|log_config| log_config.level.as_ref())
			.map(|level_str| {
				LevelFilter::from_str(level_str).map_err(|e| {
					io::Error::new(
						io::ErrorKind::InvalidInput,
						format!("Invalid log level configured: {}", e),
					)
				})
			})
			.transpose()?
			.unwrap_or(LevelFilter::Debug);

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

		#[cfg(not(feature = "experimental-lsps2-support"))]
		let lsps2_service_config: Option<LSPS2ServiceConfig> = None;
		#[cfg(feature = "experimental-lsps2-support")]
		let lsps2_service_config = Some(toml_config.liquidity
			.and_then(|l| l.lsps2_service)
			.ok_or_else(|| io::Error::new(
				io::ErrorKind::InvalidInput,
				"`liquidity.lsps2_service` must be defined in config if enabling `experimental-lsps2-support` feature."
			))?
			.into());

		let tls_config = toml_config.tls.map(|tls| TlsConfig {
			cert_path: tls.cert_path,
			key_path: tls.key_path,
			hosts: tls.hosts.unwrap_or_default(),
		});

		Ok(Config {
			listening_addrs,
			announcement_addrs,
			network: toml_config.node.network,
			alias,
			rest_service_addr,
			storage_dir_path: toml_config.storage.and_then(|s| s.disk.and_then(|d| d.dir_path)),
			chain_source,
			rabbitmq_connection_string,
			rabbitmq_exchange_name,
			lsps2_service_config,
			log_level,
			log_file_path: toml_config.log.and_then(|l| l.file),
			tls_config,
		})
	}
}

/// Configuration loaded from a TOML file.
#[derive(Deserialize, Serialize)]
pub struct TomlConfig {
	node: NodeConfig,
	storage: Option<StorageConfig>,
	bitcoind: Option<BitcoindConfig>,
	electrum: Option<ElectrumConfig>,
	esplora: Option<EsploraConfig>,
	rabbitmq: Option<RabbitmqConfig>,
	liquidity: Option<LiquidityConfig>,
	log: Option<LogConfig>,
	tls: Option<TomlTlsConfig>,
}

#[derive(Deserialize, Serialize)]
struct NodeConfig {
	network: Network,
	listening_addresses: Option<Vec<String>>,
	announcement_addresses: Option<Vec<String>>,
	rest_service_address: String,
	alias: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct StorageConfig {
	disk: Option<DiskConfig>,
}

#[derive(Deserialize, Serialize)]
struct DiskConfig {
	dir_path: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct BitcoindConfig {
	rpc_address: String,
	rpc_user: String,
	rpc_password: String,
}

#[derive(Deserialize, Serialize)]
struct ElectrumConfig {
	server_url: String,
}

#[derive(Deserialize, Serialize)]
struct EsploraConfig {
	server_url: String,
}

#[derive(Deserialize, Serialize)]
struct LogConfig {
	level: Option<String>,
	file: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct RabbitmqConfig {
	connection_string: String,
	exchange_name: String,
}

#[derive(Deserialize, Serialize)]
struct TomlTlsConfig {
	cert_path: Option<String>,
	key_path: Option<String>,
	hosts: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize)]
struct LiquidityConfig {
	lsps2_service: Option<LSPS2ServiceTomlConfig>,
}

#[derive(Deserialize, Serialize, Debug)]
struct LSPS2ServiceTomlConfig {
	advertise_service: bool,
	channel_opening_fee_ppm: u32,
	channel_over_provisioning_ppm: u32,
	min_channel_opening_fee_msat: u64,
	min_channel_lifetime: u32,
	max_client_to_self_delay: u32,
	min_payment_size_msat: u64,
	max_payment_size_msat: u64,
	client_trusts_lsp: bool,
	require_token: Option<String>,
}

impl From<LSPS2ServiceTomlConfig> for LSPS2ServiceConfig {
	fn from(val: LSPS2ServiceTomlConfig) -> Self {
		let LSPS2ServiceTomlConfig {
			advertise_service,
			channel_opening_fee_ppm,
			channel_over_provisioning_ppm,
			min_channel_opening_fee_msat,
			min_channel_lifetime,
			max_client_to_self_delay,
			min_payment_size_msat,
			max_payment_size_msat,
			client_trusts_lsp,
			require_token,
		} = val;

		Self {
			advertise_service,
			channel_opening_fee_ppm,
			channel_over_provisioning_ppm,
			min_channel_opening_fee_msat,
			min_channel_lifetime,
			min_payment_size_msat,
			max_client_to_self_delay,
			max_payment_size_msat,
			client_trusts_lsp,
			require_token,
		}
	}
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
	Config::try_from(toml_config)
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use ldk_node::bitcoin::Network;
	use ldk_node::lightning::ln::msgs::SocketAddress;

	use super::*;

	#[test]
	fn test_read_toml_config_from_file() {
		let storage_path = std::env::temp_dir();
		let config_file_name = "config.toml";

		let toml_config = r#"
			[node]
			network = "regtest"
			listening_addresses = ["localhost:3001"]
			announcement_addresses = ["54.3.7.81:3001"]
			rest_service_address = "127.0.0.1:3002"
			alias = "LDK Server"

			[tls]
			cert_path = "/path/to/tls.crt"
			key_path = "/path/to/tls.key"
			hosts = ["example.com", "ldk-server.local"]

			[storage.disk]
			dir_path = "/tmp"

			[log]
			level = "Trace"
			file = "/var/log/ldk-server.log"

			[esplora]
			server_url = "https://mempool.space/api"

			[rabbitmq]
			connection_string = "rabbitmq_connection_string"
			exchange_name = "rabbitmq_exchange_name"

			[liquidity.lsps2_service]
			advertise_service = false
			channel_opening_fee_ppm = 1000            # 0.1% fee
			channel_over_provisioning_ppm = 500000    # 50% extra capacity
			min_channel_opening_fee_msat = 10000000   # 10,000 satoshis
			min_channel_lifetime = 4320               # ~30 days
			max_client_to_self_delay = 1440           # ~10 days
			min_payment_size_msat = 10000000          # 10,000 satoshis
			max_payment_size_msat = 25000000000       # 0.25 BTC
			client_trusts_lsp = true
			"#;

		fs::write(storage_path.join(config_file_name), toml_config).unwrap();

		let mut bytes = [0u8; 32];
		let alias = "LDK Server";
		bytes[..alias.len()].copy_from_slice(alias.as_bytes());

		let config = load_config(storage_path.join(config_file_name)).unwrap();
		let expected = Config {
			listening_addrs: Some(vec![SocketAddress::from_str("localhost:3001").unwrap()]),
			announcement_addrs: Some(vec![SocketAddress::from_str("54.3.7.81:3001").unwrap()]),
			alias: Some(NodeAlias(bytes)),
			network: Network::Regtest,
			rest_service_addr: SocketAddr::from_str("127.0.0.1:3002").unwrap(),
			storage_dir_path: Some("/tmp".to_string()),
			tls_config: Some(TlsConfig {
				cert_path: Some("/path/to/tls.crt".to_string()),
				key_path: Some("/path/to/tls.key".to_string()),
				hosts: vec!["example.com".to_string(), "ldk-server.local".to_string()],
			}),
			chain_source: ChainSource::Esplora {
				server_url: String::from("https://mempool.space/api"),
			},
			rabbitmq_connection_string: "rabbitmq_connection_string".to_string(),
			rabbitmq_exchange_name: "rabbitmq_exchange_name".to_string(),
			lsps2_service_config: Some(LSPS2ServiceConfig {
				require_token: None,
				advertise_service: false,
				channel_opening_fee_ppm: 1000,
				channel_over_provisioning_ppm: 500000,
				min_channel_opening_fee_msat: 10000000,
				min_channel_lifetime: 4320,
				max_client_to_self_delay: 1440,
				min_payment_size_msat: 10000000,
				max_payment_size_msat: 25000000000,
				client_trusts_lsp: true,
			}),
			log_level: LevelFilter::Trace,
			log_file_path: Some("/var/log/ldk-server.log".to_string()),
		};

		assert_eq!(config.listening_addrs, expected.listening_addrs);
		assert_eq!(config.announcement_addrs, expected.announcement_addrs);
		assert_eq!(config.alias, expected.alias);
		assert_eq!(config.network, expected.network);
		assert_eq!(config.rest_service_addr, expected.rest_service_addr);
		assert_eq!(config.storage_dir_path, expected.storage_dir_path);
		assert_eq!(config.tls_config, expected.tls_config);
		let ChainSource::Esplora { server_url } = config.chain_source else {
			panic!("unexpected config chain source");
		};
		let ChainSource::Esplora { server_url: expected_server_url } = expected.chain_source else {
			panic!("unexpected chain source");
		};
		assert_eq!(server_url, expected_server_url);
		assert_eq!(config.rabbitmq_connection_string, expected.rabbitmq_connection_string);
		assert_eq!(config.rabbitmq_exchange_name, expected.rabbitmq_exchange_name);
		#[cfg(feature = "experimental-lsps2-support")]
		assert_eq!(config.lsps2_service_config.is_some(), expected.lsps2_service_config.is_some());
		assert_eq!(config.log_level, expected.log_level);
		assert_eq!(config.log_file_path, expected.log_file_path);

		// Test case where only electrum is set

		let toml_config = r#"
			[node]
			network = "regtest"
			rest_service_address = "127.0.0.1:3002"
			alias = "LDK Server"

			[storage.disk]
			dir_path = "/tmp"

			[log]
			level = "Trace"
			file = "/var/log/ldk-server.log"

			[electrum]
			server_url = "ssl://electrum.blockstream.info:50002"

			[rabbitmq]
			connection_string = "rabbitmq_connection_string"
			exchange_name = "rabbitmq_exchange_name"

			[liquidity.lsps2_service]
			advertise_service = false
			channel_opening_fee_ppm = 1000            # 0.1% fee
			channel_over_provisioning_ppm = 500000    # 50% extra capacity
			min_channel_opening_fee_msat = 10000000   # 10,000 satoshis
			min_channel_lifetime = 4320               # ~30 days
			max_client_to_self_delay = 1440           # ~10 days
			min_payment_size_msat = 10000000          # 10,000 satoshis
			max_payment_size_msat = 25000000000       # 0.25 BTC
			client_trusts_lsp = true
			"#;

		fs::write(storage_path.join(config_file_name), toml_config).unwrap();
		let config = load_config(storage_path.join(config_file_name)).unwrap();

		let ChainSource::Electrum { server_url } = config.chain_source else {
			panic!("unexpected chain source");
		};

		assert_eq!(server_url, "ssl://electrum.blockstream.info:50002");

		// Test case where only bitcoind is set

		let toml_config = r#"
			[node]
			network = "regtest"
			rest_service_address = "127.0.0.1:3002"
			alias = "LDK Server"

			[storage.disk]
			dir_path = "/tmp"

			[log]
			level = "Trace"
			file = "/var/log/ldk-server.log"

			[bitcoind]
			rpc_address = "127.0.0.1:8332"    # RPC endpoint
			rpc_user = "bitcoind-testuser"
			rpc_password = "bitcoind-testpassword"

			[rabbitmq]
			connection_string = "rabbitmq_connection_string"
			exchange_name = "rabbitmq_exchange_name"

			[liquidity.lsps2_service]
			advertise_service = false
			channel_opening_fee_ppm = 1000            # 0.1% fee
			channel_over_provisioning_ppm = 500000    # 50% extra capacity
			min_channel_opening_fee_msat = 10000000   # 10,000 satoshis
			min_channel_lifetime = 4320               # ~30 days
			max_client_to_self_delay = 1440           # ~10 days
			min_payment_size_msat = 10000000          # 10,000 satoshis
			max_payment_size_msat = 25000000000       # 0.25 BTC
			client_trusts_lsp = true
			"#;

		fs::write(storage_path.join(config_file_name), toml_config).unwrap();
		let config = load_config(storage_path.join(config_file_name)).unwrap();

		let ChainSource::Rpc { rpc_address, rpc_user, rpc_password } = config.chain_source else {
			panic!("unexpected chain source");
		};

		assert_eq!(rpc_address, SocketAddr::from_str("127.0.0.1:8332").unwrap());
		assert_eq!(rpc_user, "bitcoind-testuser");
		assert_eq!(rpc_password, "bitcoind-testpassword");

		// Test case where both bitcoind and esplora are set, resulting in an error

		let toml_config = r#"
			[node]
			network = "regtest"
			rest_service_address = "127.0.0.1:3002"
			alias = "LDK Server"

			[storage.disk]
			dir_path = "/tmp"

			[log]
			level = "Trace"
			file = "/var/log/ldk-server.log"

			[bitcoind]
			rpc_address = "127.0.0.1:8332"    # RPC endpoint
			rpc_user = "bitcoind-testuser"
			rpc_password = "bitcoind-testpassword"

			[esplora]
			server_url = "https://mempool.space/api"

			[rabbitmq]
			connection_string = "rabbitmq_connection_string"
			exchange_name = "rabbitmq_exchange_name"

			[liquidity.lsps2_service]
			advertise_service = false
			channel_opening_fee_ppm = 1000            # 0.1% fee
			channel_over_provisioning_ppm = 500000    # 50% extra capacity
			min_channel_opening_fee_msat = 10000000   # 10,000 satoshis
			min_channel_lifetime = 4320               # ~30 days
			max_client_to_self_delay = 1440           # ~10 days
			min_payment_size_msat = 10000000          # 10,000 satoshis
			max_payment_size_msat = 25000000000       # 0.25 BTC
			client_trusts_lsp = true
			"#;

		fs::write(storage_path.join(config_file_name), toml_config).unwrap();
		let error = load_config(storage_path.join(config_file_name)).unwrap_err();
		assert_eq!(error.to_string(), "Must set a single chain source, multiple were configured");
	}
}
