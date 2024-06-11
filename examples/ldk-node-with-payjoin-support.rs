use ldk_node::bitcoin::Network;
use ldk_node::{Builder, LogLevel};

fn main() {
	let mut builder = Builder::new();
	builder.set_log_level(LogLevel::Gossip);
	builder.set_network(Network::Testnet);
	builder.set_esplora_server("https://blockstream.info/testnet/api".to_string());
	builder.set_gossip_source_rgs(
		"https://rapidsync.lightningdevkit.org/testnet/snapshot".to_string(),
	);

	// Payjoin directory is needed only if you are setting up Payjoin receiver,
	// not required for Payjoin sender.
	let payjoin_directory = "https://payjo.in".to_string();
	// Payjoin relay is required for both Payjoin receiver and sender.
	let payjoin_relay = "https://pj.bobspacebkk.com".to_string();

	// Enable sending payjoin transactions
	// builder.set_payjoin_sender_config(payjoin_relay.clone());
	// ohttp keys refer to the Payjoin directory keys that are needed for the Payjoin receiver
	// enrollement. If those keys are not provided the node will attempt to fetch them for you.
	// let ohttp_keys = None;
	// Enable receiving payjoin transactions
	builder.set_payjoin_config(payjoin_directory, payjoin_relay);

	let node = builder.build().unwrap();

	node.start().unwrap();

	// Receiving payjoin transaction
	let payjoin_payment = node.payjoin_payment();
	let amount_to_receive = bitcoin::Amount::from_sat(1000);
	let payjoin_uri = payjoin_payment.receive(amount_to_receive).unwrap();
	let payjoin_uri = payjoin_uri.to_string();

	println!("Payjoin URI: {}", payjoin_uri);

	//** Open a channel from incoming payjoin transactions ***//
	// let payjoin_payment = node.payjoin_payment();
	// let channel_amount_sats = bitcoin::Amount::from_sat(10000);
	// use bitcoin::secp256k1::PublicKey;
	// use lightning::ln::msgs::SocketAddress;
	// let counterparty_node_id: PublicKey = unimplemented!();
	// let counterparty_address: SocketAddress = unimplemented!();
	// let payjoin_uri = match payjoin_payment.receive_with_channel_opening(channel_amount_sats, None, true,
	// 	counterparty_node_id, counterparty_address,
	// ).await {
	// 	Ok(a) => a,
	// 	Err(e) => {
	// 		panic!("{}", e);
	// 	},
	// };
	// let payjoin_uri = payjoin_uri.to_string();
	// println!("Payjoin URI: {}", payjoin_uri);

	//** Sending payjoin transaction **//
	// let payjoin_uri = payjoin::Uri::try_from(payjoin_uri).unwrap();
	// match payjoin_payment.send(payjoin_uri, None, None).await {
	// 	Ok(Some(txid)) => {
	//		dbg!("Sent transaction and got a response. Transaction completed")
	// 	},
	// 	Ok(None) => {
	//		dbg!("Sent transaction and got no response. We will keep polling the response for the next 24hours")
	// 	},
	// 	Err(e) => {
	// 		dbg!(e);
	// 	}
	// }
	node.stop().unwrap();
}
