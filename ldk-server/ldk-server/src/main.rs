mod api;
mod io;
mod service;
mod util;

use crate::service::NodeService;

use ldk_node::{Builder, Event, LogLevel};

use tokio::net::TcpListener;
use tokio::signal::unix::SignalKind;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;

use crate::util::config::load_config;
use ldk_node::config::Config;
use std::path::Path;
use std::sync::Arc;

fn main() {
	let args: Vec<String> = std::env::args().collect();

	if args.len() < 2 {
		eprintln!("Usage: {} config_path", args[0]);
		std::process::exit(-1);
	}

	let mut ldk_node_config = Config::default();
	let config_file = load_config(Path::new(&args[1])).expect("Invalid configuration file.");

	ldk_node_config.log_level = LogLevel::Trace;
	ldk_node_config.storage_dir_path = config_file.storage_dir_path;
	ldk_node_config.listening_addresses = Some(vec![config_file.listening_addr]);
	ldk_node_config.network = config_file.network;

	let mut builder = Builder::from_config(ldk_node_config);

	let bitcoind_rpc_addr = config_file.bitcoind_rpc_addr;

	builder.set_chain_source_bitcoind_rpc(
		bitcoind_rpc_addr.ip().to_string(),
		bitcoind_rpc_addr.port(),
		config_file.bitcoind_rpc_user,
		config_file.bitcoind_rpc_password,
	);

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
		let rest_svc_listener = TcpListener::bind(config_file.rest_service_addr)
			.await
			.expect("Failed to bind listening port");
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
