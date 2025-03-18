mod api;
mod io;
mod service;
mod util;

use crate::service::NodeService;

use ldk_node::{Builder, Event, Node};

use tokio::net::TcpListener;
use tokio::signal::unix::SignalKind;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;

use crate::io::events::event_publisher::{EventPublisher, NoopEventPublisher};
use crate::io::persist::paginated_kv_store::PaginatedKVStore;
use crate::io::persist::sqlite_store::SqliteStore;
use crate::io::persist::{
	FORWARDED_PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,
	FORWARDED_PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE, PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,
	PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::util::config::load_config;
use crate::util::proto_adapter::{forwarded_payment_to_proto, payment_to_proto};
use hex::DisplayHex;
use ldk_node::config::Config;
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::logger::LogLevel;
use ldk_server_protos::events;
use ldk_server_protos::events::{event_envelope, EventEnvelope};
use prost::Message;
use rand::Rng;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const USAGE_GUIDE: &str = "Usage: ldk-server <config_path>";

fn main() {
	let args: Vec<String> = std::env::args().collect();

	if args.len() < 2 {
		eprintln!("{USAGE_GUIDE}");
		std::process::exit(-1);
	}

	let arg = args[1].as_str();
	if arg == "-h" || arg == "--help" {
		println!("{}", USAGE_GUIDE);
		std::process::exit(0);
	}

	if fs::File::open(arg).is_err() {
		eprintln!("Unable to access configuration file.");
		std::process::exit(-1);
	}

	let mut ldk_node_config = Config::default();
	let config_file = load_config(Path::new(arg)).expect("Invalid configuration file.");

	ldk_node_config.storage_dir_path = config_file.storage_dir_path.clone();
	ldk_node_config.listening_addresses = Some(vec![config_file.listening_addr]);
	ldk_node_config.network = config_file.network;

	let mut builder = Builder::from_config(ldk_node_config);
	builder.set_log_facade_logger(Some(LogLevel::Trace));

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

	let paginated_store =
		Arc::new(match SqliteStore::new(PathBuf::from(config_file.storage_dir_path), None, None) {
			Ok(store) => store,
			Err(e) => {
				eprintln!("Failed to create SqliteStore: {:?}", e);
				std::process::exit(-1);
			},
		});

	let event_publisher: Arc<dyn EventPublisher> = Arc::new(NoopEventPublisher);

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
							event_node.event_handled();
						},
						Event::ChannelReady { channel_id, counterparty_node_id, .. } => {
							println!(
								"CHANNEL_READY: {} from counterparty {:?}",
								channel_id, counterparty_node_id
							);
							event_node.event_handled();
						},
						Event::PaymentReceived { payment_id, payment_hash, amount_msat, .. } => {
							println!(
								"PAYMENT_RECEIVED: with id {:?}, hash {}, amount_msat {}",
								payment_id, payment_hash, amount_msat
							);
							let payment_id = payment_id.expect("PaymentId expected for ldk-server >=0.1");
							upsert_payment_details(&event_node, Arc::clone(&paginated_store) as Arc<dyn PaginatedKVStore>, &payment_id);
						},
						Event::PaymentSuccessful {payment_id, ..} => {
							let payment_id = payment_id.expect("PaymentId expected for ldk-server >=0.1");
							upsert_payment_details(&event_node, Arc::clone(&paginated_store) as Arc<dyn PaginatedKVStore>, &payment_id);
						},
						Event::PaymentFailed {payment_id, ..} => {
							let payment_id = payment_id.expect("PaymentId expected for ldk-server >=0.1");
							upsert_payment_details(&event_node, Arc::clone(&paginated_store) as Arc<dyn PaginatedKVStore>, &payment_id);
						},
						Event::PaymentClaimable {payment_id, ..} => {
							upsert_payment_details(&event_node, Arc::clone(&paginated_store) as Arc<dyn PaginatedKVStore>, &payment_id);
						},
						Event::PaymentForwarded {
							prev_channel_id,
							next_channel_id,
							prev_user_channel_id,
							next_user_channel_id,
							prev_node_id,
							next_node_id,
							total_fee_earned_msat,
							skimmed_fee_msat,
							claim_from_onchain_tx,
							outbound_amount_forwarded_msat
						} => {

							println!("PAYMENT_FORWARDED: with outbound_amount_forwarded_msat {}, total_fee_earned_msat: {}, inbound channel: {}, outbound channel: {}",
								outbound_amount_forwarded_msat.unwrap_or(0), total_fee_earned_msat.unwrap_or(0), prev_channel_id, next_channel_id
							);

							let forwarded_payment = forwarded_payment_to_proto(
								prev_channel_id,
								next_channel_id,
								prev_user_channel_id,
								next_user_channel_id,
								prev_node_id,
								next_node_id,
								total_fee_earned_msat,
								skimmed_fee_msat,
								claim_from_onchain_tx,
								outbound_amount_forwarded_msat
							);

							// We don't expose this payment-id to the user, it is a temporary measure to generate
							// some unique identifiers until we have forwarded-payment-id available in ldk.
							// Currently, this is the expected user handling behaviour for forwarded payments.
							let mut forwarded_payment_id = [0u8;32];
							rand::thread_rng().fill(&mut forwarded_payment_id);

							let forwarded_payment_creation_time = SystemTime::now().duration_since(UNIX_EPOCH).expect("Time must be > 1970").as_secs() as i64;

							match event_publisher.publish(EventEnvelope {
									event: Some(event_envelope::Event::PaymentForwarded(events::PaymentForwarded {
											forwarded_payment: Some(forwarded_payment.clone()),
									})),
							}).await {
								Ok(_) => {},
								Err(e) => {
									println!("Failed to publish 'PaymentForwarded' event: {}", e);
									continue;
								}
							};

							match paginated_store.write(FORWARDED_PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,FORWARDED_PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
								&forwarded_payment_id.to_lower_hex_string(),
								forwarded_payment_creation_time,
								&forwarded_payment.encode_to_vec(),
							) {
								Ok(_) => {
										event_node.event_handled();
								}
								Err(e) => {
										println!("Failed to write forwarded payment to persistence: {}", e);
								}
							}
						},
						_ => {
							event_node.event_handled();
						},
					}

				},
				res = rest_svc_listener.accept() => {
					match res {
						Ok((stream, _)) => {
							let io_stream = TokioIo::new(stream);
							let node_service = NodeService::new(Arc::clone(&node), Arc::clone(&paginated_store) as Arc<dyn PaginatedKVStore + Send + Sync>);
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

fn upsert_payment_details(
	event_node: &Node, paginated_store: Arc<dyn PaginatedKVStore>, payment_id: &PaymentId,
) {
	if let Some(payment_details) = event_node.payment(payment_id) {
		let payment = payment_to_proto(payment_details);
		let time =
			SystemTime::now().duration_since(UNIX_EPOCH).expect("Time must be > 1970").as_secs()
				as i64;

		match paginated_store.write(
			PAYMENTS_PERSISTENCE_PRIMARY_NAMESPACE,
			PAYMENTS_PERSISTENCE_SECONDARY_NAMESPACE,
			&payment_id.0.to_lower_hex_string(),
			time,
			&payment.encode_to_vec(),
		) {
			Ok(_) => {
				event_node.event_handled();
			},
			Err(e) => {
				eprintln!("Failed to write payment to persistence: {}", e);
			},
		}
	} else {
		eprintln!("Unable to find payment with paymentId: {}", payment_id.0.to_lower_hex_string());
	}
}
