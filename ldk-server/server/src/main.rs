mod api;
mod io;
mod service;
mod util;

use crate::service::NodeService;

use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, Event, LogLevel};

use tokio::net::TcpListener;
use tokio::signal::unix::SignalKind;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;

use ldk_node::config::Config;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

fn main() {
	let args: Vec<String> = std::env::args().collect();

	if args.len() < 6 {
		eprintln!(
			"Usage: {} storage_path listening_addr rest_svc_addr network esplora_server_url",
			args[0]
		);
		std::process::exit(-1);
	}

	let mut config = Config::default();
	config.storage_dir_path = args[1].clone();
	config.log_level = LogLevel::Trace;

	config.listening_addresses = match SocketAddress::from_str(&args[2]) {
		Ok(addr) => Some(vec![addr]),
		Err(_) => {
			eprintln!("Failed to parse listening_addr: {}", args[2]);
			std::process::exit(-1);
		},
	};

	let rest_svc_addr = match SocketAddr::from_str(&args[3]) {
		Ok(addr) => addr,
		Err(_) => {
			eprintln!("Failed to parse rest_svc_addr: {}", args[3]);
			std::process::exit(-1);
		},
	};

	config.network = match Network::from_str(&args[4]) {
		Ok(network) => network,
		Err(_) => {
			eprintln!("Unsupported network: {}. Use 'bitcoin', 'testnet', 'regtest', 'signet', 'regtest'.", args[4]);
			std::process::exit(-1);
		},
	};

	let mut builder = Builder::from_config(config);
	builder.set_chain_source_esplora(args[5].clone(), None);

	let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
		Ok(runtime) => Arc::new(runtime),
		Err(e) => {
			eprintln!("Failed to setup tokio runtime: {}", e);
			std::process::exit(-1);
		},
	};

	let node = match builder.build() {
		Ok(node) => Arc::new(node),
		Err(e) => {
			eprintln!("Failed to build LDK Node: {}", e);
			std::process::exit(-1);
		},
	};

	println!("Starting up...");
	match node.start_with_runtime(Arc::clone(&runtime)) {
		Ok(()) => {},
		Err(e) => {
			eprintln!("Failed to start up LDK Node: {}", e);
			std::process::exit(-1);
		},
	}

	println!(
		"CONNECTION_STRING: {}@{}",
		node.node_id(),
		node.config().listening_addresses.as_ref().unwrap().first().unwrap()
	);

	runtime.block_on(async {
		let mut sigterm_stream = match tokio::signal::unix::signal(SignalKind::terminate()) {
			Ok(stream) => stream,
			Err(e) => {
				println!("Failed to register for SIGTERM stream: {}", e);
				std::process::exit(-1);
			},
		};
		let event_node = Arc::clone(&node);
		let rest_svc_listener =
			TcpListener::bind(rest_svc_addr).await.expect("Failed to bind listening port");
		loop {
			tokio::select! {
				event = event_node.next_event_async() => {
					match event {
						Event::ChannelPending { channel_id, counterparty_node_id, .. } => {
							println!(
								"CHANNEL_PENDING: {} from counterparty {}",
								channel_id, counterparty_node_id
							);
						},
						Event::ChannelReady { channel_id, counterparty_node_id, .. } => {
							println!(
								"CHANNEL_READY: {} from counterparty {:?}",
								channel_id, counterparty_node_id
							);
						},
						Event::PaymentReceived { payment_id, payment_hash, amount_msat } => {
							println!(
								"PAYMENT_RECEIVED: with id {:?}, hash {}, amount_msat {}",
								payment_id, payment_hash, amount_msat
							);
						},
						_ => {},
					}
					event_node.event_handled();
				},
				res = rest_svc_listener.accept() => {
					match res {
						Ok((stream, _)) => {
							let io_stream = TokioIo::new(stream);
							let node_service = NodeService::new(Arc::clone(&node));
							runtime.spawn(async move {
								if let Err(err) = http1::Builder::new().serve_connection(io_stream, node_service).await {
									eprintln!("Failed to serve connection: {}", err);
								}
							});
						},
						Err(e) => eprintln!("Failed to accept connection: {}", e),
					}
				}
				_ = tokio::signal::ctrl_c() => {
					println!("Received CTRL-C, shutting down..");
					break;
				}
				_ = sigterm_stream.recv() => {
					println!("Received SIGTERM, shutting down..");
					break;
				}
			}
		}
	});

	node.stop().expect("Shutdown should always succeed.");
	println!("Shutdown complete..");
}
