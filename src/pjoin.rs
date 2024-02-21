/// This is an implementation of the payjoin protocol using `pj_new_crate` and `payjoin` crates.
///
/// Payjoin is used in the context of channel opening, allowing a node to fund a channel using
/// funds from an incoming payjoin request.
use bitcoin::secp256k1::{self, PublicKey, Secp256k1};
use lightning::util::persist::KVStore;
use tokio::sync::Mutex;

use crate::{
	logger::FilesystemLogger,
	pj_new_crate::{PayjoinExecuter, PayjoinScheduler, ScheduledChannel},
	types::{ChannelManager, PeerManager, Wallet},
};
use std::sync::Arc;

pub struct LDKPayjoinExecuter<K: KVStore + Sync + Send + 'static> {
	channel_manager: Arc<ChannelManager<K>>,
	logger: Arc<FilesystemLogger>,
	peer_manager: Arc<PeerManager<K>>,
	wallet: Arc<Wallet>,
}

impl<K> Clone for LDKPayjoinExecuter<K>
where
	K: KVStore + Sync + Send + 'static,
{
	fn clone(&self) -> Self {
		Self {
			channel_manager: self.channel_manager.clone(),
			logger: self.logger.clone(),
			peer_manager: self.peer_manager.clone(),
			wallet: self.wallet.clone(),
		}
	}
}

impl<K: KVStore + Sync + Send + 'static> LDKPayjoinExecuter<K> {
	pub fn new(
		wallet: Arc<Wallet>, logger: Arc<FilesystemLogger>, peer_manager: Arc<PeerManager<K>>,
		channel_manager: Arc<ChannelManager<K>>,
	) -> Self {
		Self { wallet, logger, peer_manager, channel_manager }
	}
}

impl<K: KVStore + Sync + Send + 'static> PayjoinExecuter for LDKPayjoinExecuter<K> {
	async fn request_to_psbt(
		&self, _channel: ScheduledChannel, request: String,
	) -> Result<String, Box<dyn std::error::Error>> {
		// unimplemented!();
		Ok(request)
	}
}

pub struct LDKPayjoin<K: KVStore + Sync + Send + 'static> {
	scheduler: Arc<Mutex<PayjoinScheduler<LDKPayjoinExecuter<K>>>>,
}

impl<K: KVStore + Sync + Send + 'static> LDKPayjoin<K> {
	pub fn new(executer: LDKPayjoinExecuter<K>) -> Self {
		// just for testing
		let test_pubkey = || -> PublicKey {
			let secp = Secp256k1::new();
			PublicKey::from_secret_key(&secp, &secp256k1::SecretKey::from_slice(&[1; 32]).unwrap())
		};
		let test_channels = vec![ScheduledChannel::new(10_000, Some(1_000), true, test_pubkey())];
		// let channels = Vec::new();
		let payjoin_scheduler = PayjoinScheduler::new(test_channels, executer);
		Self { scheduler: Arc::new(Mutex::new(payjoin_scheduler)) }
	}

	pub async fn schedule(&self, channel: ScheduledChannel) {
		self.scheduler.lock().await.schedule(channel);
	}

	pub async fn list_scheduled_channels(&self) -> Vec<ScheduledChannel> {
		self.scheduler.lock().await.list_scheduled_channels()
	}

	pub async fn serve(&self, stream: tokio::net::TcpStream) -> Result<(), tokio::task::JoinError> {
		self.scheduler.lock().await.serve(stream).await
	}
}
