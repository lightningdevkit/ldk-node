/// Payjoin is a protocol for improving the privacy of Bitcoin transactions.  It allows the
/// receiver of a Bitcoin payment to add inputs to the transaction, making it look like a regular
/// payment.  This makes it harder for blockchain analysis to determine which transaction outputs
/// are change and which are payments.
///
/// In Lightning Network, the payjoin protocol can be used to receive payment to your bitcoin
/// wallet and fund a channel at the same transaction. This can save on-chain fees.
///
/// This module provides `PayjoinScheduler` and a `PayjoinExecuter` trait that can be used to
/// implement the payjoin protocol in a Lightning Network node.
use bitcoin::secp256k1::PublicKey;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use rand::Rng;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::task::JoinError;

/// `PayjoinExecuter` is a trait that defines an interface for executing payjoin requests in
/// Lightning Network environment where it tries to create a channel with a predefined channel
/// details and use the incoming payjoin payment to fund the channel.
///
/// `PayjoinExecuter` is used by `PayjoinScheduler` to execute on the incoming payjoin requests and
/// schedulded channels.
pub trait PayjoinExecuter {
	/// The `request_to_psbt` method is called when a payjoin request is received. The method should
	/// return a PSBT that is the result of the negotiation with a counterparty node after they
	/// responded with FundingSigned message.
	fn request_to_psbt(
		&self, channel: ScheduledChannel, request: String,
	) -> impl std::future::Future<Output = Result<String, Box<dyn std::error::Error>>> + std::marker::Send;
}

/// A scheduled channel is a channel that is scheduled to be created with a counterparty node. The
/// channel is opened when a payjoin request is received and the channel is funded with the
/// incoming payment.
#[derive(Clone, Debug)]
pub struct ScheduledChannel {
	channel_amount_sats: u64,
	push_msat: Option<u64>,
	user_channel_id: u128,
	announce_channel: bool,
	node_id: PublicKey,
}

impl ScheduledChannel {
	/// Create a new `ScheduledChannel` with the given channel details.
	pub fn new(
		channel_amount_sats: u64, push_msat: Option<u64>, announce_channel: bool,
		node_id: PublicKey,
	) -> Self {
		let user_channel_id: u128 = rand::thread_rng().gen::<u128>();
		Self { channel_amount_sats, push_msat, user_channel_id, announce_channel, node_id }
	}
	/// Get the channel amount in satoshis.
	///
	/// The channel amount is the amount that is used to fund the channel when it is created.
	pub fn channel_amount_sats(&self) -> u64 {
		self.channel_amount_sats
	}
	/// Get the push amount in millisatoshis.
	///
	/// The push amount is the amount that is pushed to the counterparty node when the channel is
	/// created.
	pub fn push_msat(&self) -> Option<u64> {
		self.push_msat
	}
	/// Get the user channel id.
	pub fn user_channel_id(&self) -> u128 {
		self.user_channel_id
	}
	/// Get the announce channel flag.
	///
	/// The announce channel flag is used to determine if the channel should be announced to the
	/// network when it is created.
	pub fn announce_channel(&self) -> bool {
		self.announce_channel
	}
	/// Get the node id of the counterparty node.
	///
	/// The node id is the public key of the counterparty node that is used to create the channel.
	pub fn node_id(&self) -> PublicKey {
		self.node_id
	}
}

/// `PayjoinScheduler` is a scheduler that handles the incoming payjoin requests and the channels
/// that are to be created with the counterparty node.
///
/// It manages a list of `ScheduledChannel` and executes on the incoming payjoin requests using the
/// `PayjoinExecuter` trait.
#[derive(Clone)]
pub struct PayjoinScheduler<P: PayjoinExecuter + Send + Sync + 'static + Clone>(
	Vec<ScheduledChannel>,
	P,
);

impl<P> PayjoinScheduler<P>
where
	P: PayjoinExecuter + Send + Sync + 'static + Clone,
{
	/// Create a new `PayjoinScheduler` with the given channels and executer.
	pub fn new(channels: Vec<ScheduledChannel>, executer: P) -> Self {
		Self(channels, executer)
	}

	/// Schedule a new channel to be created with the counterparty node.
	///
	/// The channel is added to the list of scheduled channels and is used to create a channel when
	/// a payjoin request is received.
	pub fn schedule(&mut self, channel: ScheduledChannel) {
		self.0.push(channel);
	}

	/// List the scheduled channels.
	pub fn list_scheduled_channels(&self) -> Vec<ScheduledChannel> {
		self.0.clone()
	}

	/// Pop the scheduled channel from the list of scheduled channels.
	///
	/// The channel is removed from the list of scheduled channels and is used to create a channel
	/// when a payjoin request is received.
	pub fn pop_scheduled_channel(&mut self) -> Option<ScheduledChannel> {
		self.0.pop()
	}

	/// Execute on the incoming payjoin request.
	pub async fn request_to_psbt(
		&self, channel: ScheduledChannel, request: String,
	) -> Result<String, Box<dyn std::error::Error>> {
		self.1.request_to_psbt(channel, request).await
	}

	/// Serve an incoming payjoin request.
	///
	/// The incoming payjoin request is served using the given `TcpStream`.
	/// The payjoin request is handled by the payjoin_handler function.
	///
	/// The `PayjoinScheduler` is shared across multiple threads using the `Arc` and `Mutex` types.
	/// And is accessible from the payjoin_handler function.
	pub async fn serve(&self, stream: TcpStream) -> Result<(), JoinError> {
		let io = TokioIo::new(stream);
		let channels = self.0.clone();
		let executer = self.1.clone();
		let payjoin_scheduler = Arc::new(Mutex::new(PayjoinScheduler::new(channels, executer)));
		tokio::task::spawn(async move {
			if let Err(err) = http1::Builder::new()
				.serve_connection(
					io,
					service_fn(move |http_request| {
						payjoin_handler(http_request, payjoin_scheduler.clone())
					}),
				)
				.await
			{
				println!("Error serving connection: {:?}", err);
			}
		})
		.await
	}
}

async fn payjoin_handler<P: PayjoinExecuter + Send + Sync + 'static + Clone>(
	http_request: Request<Incoming>, pj_scheduler: Arc<Mutex<PayjoinScheduler<P>>>,
) -> Result<hyper::Response<Full<bytes::Bytes>>, hyper::Error> {
	let make_http_response =
		|s: String| -> Result<hyper::Response<Full<bytes::Bytes>>, hyper::Error> {
			Ok(hyper::Response::builder().body(Full::new(bytes::Bytes::from(s))).unwrap())
		};
	match (http_request.method(), http_request.uri().path()) {
		(&hyper::Method::POST, "/payjoin") => {
			// integrat payjoin crate here
			let _headers = http_request.headers().clone();
			let body = http_request.into_body().collect().await?;
			let body = String::from_utf8(body.to_bytes().to_vec()).unwrap();
			let mut scheduler = pj_scheduler.lock().await;
			let channel = scheduler.pop_scheduled_channel().unwrap();
			let res = scheduler.request_to_psbt(channel, body).await.unwrap();
			return make_http_response(res);
		},
		_ => make_http_response("404".into()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use bitcoin::secp256k1::{self, Secp256k1};

	#[derive(Clone)]
	struct PayjoinExecuterImpl;

	impl PayjoinExecuter for PayjoinExecuterImpl {
		async fn request_to_psbt(
			&self, _channel: ScheduledChannel, _request: String,
		) -> Result<String, Box<dyn std::error::Error>> {
			Ok(String::new())
		}
	}

	#[tokio::test]
	async fn test_payjoin_scheduler() {
		let create_pubkey = || -> PublicKey {
			let secp = Secp256k1::new();
			PublicKey::from_secret_key(&secp, &secp256k1::SecretKey::from_slice(&[1; 32]).unwrap())
		};
		let executer = PayjoinExecuterImpl;
		let executer = executer;
		let channels = Vec::new();
		let mut scheduler = PayjoinScheduler::new(channels, executer);
		let channel_amount_sats = 100;
		let push_msat = None;
		let announce_channel = false;
		let node_id = create_pubkey();
		let channel =
			ScheduledChannel::new(channel_amount_sats, push_msat, announce_channel, node_id);
		scheduler.schedule(channel.clone());
		let channels = scheduler.list_scheduled_channels();
		assert_eq!(channels.len(), 1);
		let ch = scheduler.pop_scheduled_channel().unwrap();
		assert_eq!(channel.user_channel_id, ch.user_channel_id);
		let channels = scheduler.list_scheduled_channels();
		assert_eq!(channels.len(), 0);
	}
}
