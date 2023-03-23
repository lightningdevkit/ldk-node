use crate::{
	hex_utils, ChannelManager, Config, Error, KeysManager, NetworkGraph, PaymentDirection,
	PaymentInfo, PaymentInfoStorage, PaymentStatus, Wallet,
};

use crate::io::KVStoreUnpersister;
use crate::logger::{log_error, log_info, Logger};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::impl_writeable_tlv_based_enum;
use lightning::ln::PaymentHash;
use lightning::routing::gossip::NodeId;
use lightning::util::errors::APIError;
use lightning::util::events::Event as LdkEvent;
use lightning::util::events::EventHandler as LdkEventHandler;
use lightning::util::events::PaymentPurpose;
use lightning::util::persist::KVStorePersister;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use bitcoin::secp256k1::Secp256k1;
use rand::{thread_rng, Rng};
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The event queue will be persisted under this key.
pub(crate) const EVENTS_PERSISTENCE_KEY: &str = "events";

/// An event emitted by [`Node`], which should be handled by the user.
///
/// [`Node`]: [`crate::Node`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
	/// A sent payment was successful.
	PaymentSuccessful {
		/// The hash of the payment.
		payment_hash: PaymentHash,
	},
	/// A sent payment has failed.
	PaymentFailed {
		/// The hash of the payment.
		payment_hash: PaymentHash,
	},
	/// A payment has been received.
	PaymentReceived {
		/// The hash of the payment.
		payment_hash: PaymentHash,
		/// The value, in thousandths of a satoshi, that has been received.
		amount_msat: u64,
	},
	/// A channel is ready to be used.
	ChannelReady {
		/// The `channel_id` of the channel.
		channel_id: [u8; 32],
		/// The `user_channel_id` of the channel.
		user_channel_id: u128,
	},
	/// A channel has been closed.
	ChannelClosed {
		/// The `channel_id` of the channel.
		channel_id: [u8; 32],
		/// The `user_channel_id` of the channel.
		user_channel_id: u128,
	},
}

impl_writeable_tlv_based_enum!(Event,
	(0, PaymentSuccessful) => {
		(0, payment_hash, required),
	},
	(1, PaymentFailed) => {
		(0, payment_hash, required),
	},
	(2, PaymentReceived) => {
		(0, payment_hash, required),
		(1, amount_msat, required),
	},
	(3, ChannelReady) => {
		(0, channel_id, required),
		(1, user_channel_id, required),
	},
	(4, ChannelClosed) => {
		(0, channel_id, required),
		(1, user_channel_id, required),
	};
);

pub(crate) struct EventQueue<K: Deref>
where
	K::Target: KVStorePersister,
{
	queue: Mutex<VecDeque<Event>>,
	notifier: Condvar,
	persister: K,
}

impl<K: Deref> EventQueue<K>
where
	K::Target: KVStorePersister,
{
	pub(crate) fn new(persister: K) -> Self {
		let queue: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());
		let notifier = Condvar::new();
		Self { queue, notifier, persister }
	}

	pub(crate) fn add_event(&self, event: Event) -> Result<(), Error> {
		{
			let mut locked_queue = self.queue.lock().unwrap();
			locked_queue.push_back(event);
			self.persist_queue(&locked_queue)?;
		}

		self.notifier.notify_one();
		Ok(())
	}

	pub(crate) fn next_event(&self) -> Event {
		let locked_queue =
			self.notifier.wait_while(self.queue.lock().unwrap(), |queue| queue.is_empty()).unwrap();
		locked_queue.front().unwrap().clone()
	}

	pub(crate) fn event_handled(&self) -> Result<(), Error> {
		{
			let mut locked_queue = self.queue.lock().unwrap();
			locked_queue.pop_front();
			self.persist_queue(&locked_queue)?;
		}
		self.notifier.notify_one();
		Ok(())
	}

	fn persist_queue(&self, locked_queue: &VecDeque<Event>) -> Result<(), Error> {
		self.persister
			.persist(EVENTS_PERSISTENCE_KEY, &EventQueueSerWrapper(locked_queue))
			.map_err(|_| Error::PersistenceFailed)?;
		Ok(())
	}
}

impl<K: Deref> ReadableArgs<K> for EventQueue<K>
where
	K::Target: KVStorePersister,
{
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, persister: K,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let read_queue: EventQueueDeserWrapper = Readable::read(reader)?;
		let queue: Mutex<VecDeque<Event>> = Mutex::new(read_queue.0);
		let notifier = Condvar::new();
		Ok(Self { queue, notifier, persister })
	}
}

struct EventQueueDeserWrapper(VecDeque<Event>);

impl Readable for EventQueueDeserWrapper {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let len: u16 = Readable::read(reader)?;
		let mut queue = VecDeque::with_capacity(len as usize);
		for _ in 0..len {
			queue.push_back(Readable::read(reader)?);
		}
		Ok(Self(queue))
	}
}

struct EventQueueSerWrapper<'a>(&'a VecDeque<Event>);

impl Writeable for EventQueueSerWrapper<'_> {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		(self.0.len() as u16).write(writer)?;
		for e in self.0.iter() {
			e.write(writer)?;
		}
		Ok(())
	}
}

pub(crate) struct EventHandler<K: Deref + Clone, L: Deref>
where
	K::Target: KVStorePersister + KVStoreUnpersister,
	L::Target: Logger,
{
	wallet: Arc<Wallet<bdk::database::SqliteDatabase>>,
	event_queue: Arc<EventQueue<K>>,
	channel_manager: Arc<ChannelManager>,
	network_graph: Arc<NetworkGraph>,
	keys_manager: Arc<KeysManager>,
	payment_store: Arc<PaymentInfoStorage<K>>,
	tokio_runtime: Arc<tokio::runtime::Runtime>,
	logger: L,
	_config: Arc<Config>,
}

impl<K: Deref + Clone, L: Deref> EventHandler<K, L>
where
	K::Target: KVStorePersister + KVStoreUnpersister,
	L::Target: Logger,
{
	pub fn new(
		wallet: Arc<Wallet<bdk::database::SqliteDatabase>>, event_queue: Arc<EventQueue<K>>,
		channel_manager: Arc<ChannelManager>, network_graph: Arc<NetworkGraph>,
		keys_manager: Arc<KeysManager>, payment_store: Arc<PaymentInfoStorage<K>>,
		tokio_runtime: Arc<tokio::runtime::Runtime>, logger: L, _config: Arc<Config>,
	) -> Self {
		Self {
			event_queue,
			wallet,
			channel_manager,
			network_graph,
			keys_manager,
			payment_store,
			logger,
			tokio_runtime,
			_config,
		}
	}
}

impl<K: Deref + Clone, L: Deref> LdkEventHandler for EventHandler<K, L>
where
	K::Target: KVStorePersister + KVStoreUnpersister,
	L::Target: Logger,
{
	fn handle_event(&self, event: LdkEvent) {
		match event {
			LdkEvent::FundingGenerationReady {
				temporary_channel_id,
				counterparty_node_id,
				channel_value_satoshis,
				output_script,
				..
			} => {
				// Construct the raw transaction with the output that is paid the amount of the
				// channel.
				let confirmation_target = ConfirmationTarget::Normal;

				// Sign the final funding transaction and broadcast it.
				match self.wallet.create_funding_transaction(
					output_script,
					channel_value_satoshis,
					confirmation_target,
				) {
					Ok(final_tx) => {
						// Give the funding transaction back to LDK for opening the channel.
						match self.channel_manager.funding_transaction_generated(
							&temporary_channel_id,
							&counterparty_node_id,
							final_tx,
						) {
							Ok(()) => {}
							Err(APIError::APIMisuseError { err }) => {
								log_error!(self.logger, "Panicking due to APIMisuseError: {}", err);
								panic!("APIMisuseError: {}", err);
							}
							Err(APIError::ChannelUnavailable { err }) => {
								log_error!(
									self.logger,
									"Failed to process funding transaction as channel went away before we could fund it: {}",
									err
								)
							}
							Err(err) => {
								log_error!(
									self.logger,
									"Failed to process funding transaction: {:?}",
									err
								)
							}
						}
					}
					Err(err) => {
						log_error!(self.logger, "Failed to create funding transaction: {}", err);
						self.channel_manager
							.force_close_without_broadcasting_txn(
								&temporary_channel_id,
								&counterparty_node_id,
							)
							.expect(
								"Failed to force close channel after funding generation failed",
							);
					}
				}
			}
			LdkEvent::PaymentClaimable {
				payment_hash,
				purpose,
				amount_msat,
				receiver_node_id: _,
				via_channel_id: _,
				via_user_channel_id: _,
			} => {
				if let Some(info) = self.payment_store.get(&payment_hash) {
					if info.status == PaymentStatus::Succeeded {
						log_info!(
							self.logger,
							"Refused duplicate inbound payment from payment hash {} of {}msat",
							hex_utils::to_string(&payment_hash.0),
							amount_msat,
						);
						self.channel_manager.fail_htlc_backwards(&payment_hash);
						self.payment_store
							.set_status(&payment_hash, PaymentStatus::Failed)
							.expect("Failed to access payment store");
						return;
					}
				}

				log_info!(
					self.logger,
					"Received payment from payment hash {} of {}msat",
					hex_utils::to_string(&payment_hash.0),
					amount_msat,
				);
				let payment_preimage = match purpose {
					PaymentPurpose::InvoicePayment { payment_preimage, payment_secret } => {
						if payment_preimage.is_some() {
							payment_preimage
						} else {
							self.channel_manager
								.get_payment_preimage(payment_hash, payment_secret)
								.ok()
						}
					}
					PaymentPurpose::SpontaneousPayment(preimage) => Some(preimage),
				};

				if let Some(preimage) = payment_preimage {
					self.channel_manager.claim_funds(preimage);
				} else {
					log_error!(
						self.logger,
						"Failed to claim payment with hash {}: preimage unknown.",
						hex_utils::to_string(&payment_hash.0),
					);
					self.channel_manager.fail_htlc_backwards(&payment_hash);
					self.payment_store
						.set_status(&payment_hash, PaymentStatus::Failed)
						.expect("Failed to access payment store");
				}
			}
			LdkEvent::PaymentClaimed {
				payment_hash,
				purpose,
				amount_msat,
				receiver_node_id: _,
			} => {
				log_info!(
					self.logger,
					"Claimed payment from payment hash {} of {}msat.",
					hex_utils::to_string(&payment_hash.0),
					amount_msat,
				);
				let (payment_preimage, payment_secret) = match purpose {
					PaymentPurpose::InvoicePayment { payment_preimage, payment_secret, .. } => {
						(payment_preimage, Some(payment_secret))
					}
					PaymentPurpose::SpontaneousPayment(preimage) => (Some(preimage), None),
				};

				let mut locked_store = self.payment_store.lock().unwrap();
				locked_store
					.entry(payment_hash)
					.and_modify(|payment_info| {
						payment_info.status = PaymentStatus::Succeeded;
						payment_info.preimage = payment_preimage;
						payment_info.secret = payment_secret;
						payment_info.amount_msat = Some(amount_msat);
					})
					.or_insert(PaymentInfo {
						preimage: payment_preimage,
						payment_hash,
						secret: payment_secret,
						amount_msat: Some(amount_msat),
						direction: PaymentDirection::Inbound,
						status: PaymentStatus::Succeeded,
					});

				self.event_queue
					.add_event(Event::PaymentReceived { payment_hash, amount_msat })
					.expect("Failed to push to event queue");
			}
			LdkEvent::PaymentSent { payment_preimage, payment_hash, fee_paid_msat, .. } => {
				if let Some(mut payment_info) = self.payment_store.get(&payment_hash) {
					payment_info.preimage = Some(payment_preimage);
					payment_info.status = PaymentStatus::Succeeded;
					self.payment_store
						.insert(payment_info.clone())
						.expect("Failed to access payment store");
					log_info!(
						self.logger,
						"Successfully sent payment of {}msat{} from \
						payment hash {:?} with preimage {:?}",
						payment_info.amount_msat.unwrap(),
						if let Some(fee) = fee_paid_msat {
							format!(" (fee {} msat)", fee)
						} else {
							"".to_string()
						},
						hex_utils::to_string(&payment_hash.0),
						hex_utils::to_string(&payment_preimage.0)
					);
				}
				self.event_queue
					.add_event(Event::PaymentSuccessful { payment_hash })
					.expect("Failed to push to event queue");
			}
			LdkEvent::PaymentFailed { payment_hash, .. } => {
				log_info!(
					self.logger,
					"Failed to send payment to payment hash {:?}.",
					hex_utils::to_string(&payment_hash.0)
				);

				self.payment_store
					.set_status(&payment_hash, PaymentStatus::Failed)
					.expect("Failed to access payment store");
				self.event_queue
					.add_event(Event::PaymentFailed { payment_hash })
					.expect("Failed to push to event queue");
			}

			LdkEvent::PaymentPathSuccessful { .. } => {}
			LdkEvent::PaymentPathFailed { .. } => {}
			LdkEvent::ProbeSuccessful { .. } => {}
			LdkEvent::ProbeFailed { .. } => {}
			LdkEvent::HTLCHandlingFailed { .. } => {}
			LdkEvent::PendingHTLCsForwardable { time_forwardable } => {
				let forwarding_channel_manager = self.channel_manager.clone();
				let min = time_forwardable.as_millis() as u64;

				self.tokio_runtime.spawn(async move {
					let millis_to_sleep = thread_rng().gen_range(min..min * 5) as u64;
					tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;

					forwarding_channel_manager.process_pending_htlc_forwards();
				});
			}
			LdkEvent::SpendableOutputs { outputs } => {
				// TODO: We should eventually remember the outputs and supply them to the wallet's coin selection, once BDK allows us to do so.
				let destination_address = self.wallet.get_new_address().unwrap();
				let output_descriptors = &outputs.iter().collect::<Vec<_>>();
				let tx_feerate =
					self.wallet.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
				let res = self.keys_manager.spend_spendable_outputs(
					output_descriptors,
					Vec::new(),
					destination_address.script_pubkey(),
					tx_feerate,
					&Secp256k1::new(),
				);
				match res {
					Ok(spending_tx) => self.wallet.broadcast_transaction(&spending_tx),
					Err(err) => {
						log_error!(self.logger, "Error spending outputs: {:?}", err);
					}
				}
			}
			LdkEvent::OpenChannelRequest { .. } => {}
			LdkEvent::PaymentForwarded {
				prev_channel_id,
				next_channel_id,
				fee_earned_msat,
				claim_from_onchain_tx,
			} => {
				let read_only_network_graph = self.network_graph.read_only();
				let nodes = read_only_network_graph.nodes();
				let channels = self.channel_manager.list_channels();

				let node_str = |channel_id: &Option<[u8; 32]>| {
					channel_id
						.and_then(|channel_id| channels.iter().find(|c| c.channel_id == channel_id))
						.and_then(|channel| {
							nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id))
						})
						.map_or("private_node".to_string(), |node| {
							node.announcement_info
								.as_ref()
								.map_or("unnamed node".to_string(), |ann| {
									format!("node {}", ann.alias)
								})
						})
				};
				let channel_str = |channel_id: &Option<[u8; 32]>| {
					channel_id
						.map(|channel_id| {
							format!(" with channel {}", hex_utils::to_string(&channel_id))
						})
						.unwrap_or_default()
				};
				let from_prev_str = format!(
					" from {}{}",
					node_str(&prev_channel_id),
					channel_str(&prev_channel_id)
				);
				let to_next_str =
					format!(" to {}{}", node_str(&next_channel_id), channel_str(&next_channel_id));

				let fee_earned = fee_earned_msat.unwrap_or(0);
				if claim_from_onchain_tx {
					log_info!(
						self.logger,
						"Forwarded payment{}{}, earning {}msat in fees from claiming onchain.",
						from_prev_str,
						to_next_str,
						fee_earned,
					);
				} else {
					log_info!(
						self.logger,
						"Forwarded payment{}{}, earning {}msat in fees.",
						from_prev_str,
						to_next_str,
						fee_earned,
					);
				}
			}
			LdkEvent::ChannelReady {
				channel_id, user_channel_id, counterparty_node_id, ..
			} => {
				log_info!(
					self.logger,
					"Channel {} with {} ready to be used.",
					hex_utils::to_string(&channel_id),
					counterparty_node_id,
				);
				self.event_queue
					.add_event(Event::ChannelReady { channel_id, user_channel_id })
					.expect("Failed to push to event queue");
			}
			LdkEvent::ChannelClosed { channel_id, reason, user_channel_id } => {
				log_info!(
					self.logger,
					"Channel {} closed due to: {:?}",
					hex_utils::to_string(&channel_id),
					reason
				);
				self.event_queue
					.add_event(Event::ChannelClosed { channel_id, user_channel_id })
					.expect("Failed to push to event queue");
			}
			LdkEvent::DiscardFunding { .. } => {}
			LdkEvent::HTLCIntercepted { .. } => {}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test::utils::TestPersister;

	#[test]
	fn event_queue_persistence() {
		let persister = Arc::new(TestPersister::new());
		let event_queue = EventQueue::new(Arc::clone(&persister));

		let expected_event = Event::ChannelReady { channel_id: [23u8; 32], user_channel_id: 2323 };
		event_queue.add_event(expected_event.clone()).unwrap();
		assert!(persister.get_and_clear_did_persist());

		// Check we get the expected event and that it is returned until we mark it handled.
		for _ in 0..5 {
			assert_eq!(event_queue.next_event(), expected_event);
			assert_eq!(false, persister.get_and_clear_did_persist());
		}

		// Check we can read back what we persisted.
		let persisted_bytes = persister.get_persisted_bytes(EVENTS_PERSISTENCE_KEY).unwrap();
		let deser_event_queue =
			EventQueue::read(&mut &persisted_bytes[..], Arc::clone(&persister)).unwrap();
		assert_eq!(deser_event_queue.next_event(), expected_event);
		assert!(!persister.get_and_clear_did_persist());

		// Check we persisted on `event_handled()`
		event_queue.event_handled().unwrap();

		assert!(persister.get_and_clear_did_persist());
	}
}
