use crate::{
	hex_utils, ChannelId, ChannelManager, Config, Error, KeysManager, NetworkGraph, UserChannelId,
	Wallet,
};

use crate::payment_store::{
	PaymentDetails, PaymentDetailsUpdate, PaymentDirection, PaymentStatus, PaymentStore,
};

use crate::io::{
	KVStore, TransactionalWrite, EVENT_QUEUE_PERSISTENCE_KEY, EVENT_QUEUE_PERSISTENCE_NAMESPACE,
};
use crate::logger::{log_error, log_info, Logger};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::events::Event as LdkEvent;
use lightning::events::PaymentPurpose;
use lightning::impl_writeable_tlv_based_enum;
use lightning::ln::PaymentHash;
use lightning::routing::gossip::NodeId;
use lightning::util::errors::APIError;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use bitcoin::secp256k1::{PublicKey, Secp256k1};
use bitcoin::OutPoint;
use rand::{thread_rng, Rng};
use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

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
	/// A channel has been created and is pending confirmation on-chain.
	ChannelPending {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
		/// The `temporary_channel_id` this channel used to be known by during channel establishment.
		former_temporary_channel_id: ChannelId,
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The outpoint of the channel's funding transaction.
		funding_txo: OutPoint,
	},
	/// A channel is ready to be used.
	ChannelReady {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
	},
	/// A channel has been closed.
	ChannelClosed {
		/// The `channel_id` of the channel.
		channel_id: ChannelId,
		/// The `user_channel_id` of the channel.
		user_channel_id: UserChannelId,
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
	(4, ChannelPending) => {
		(0, channel_id, required),
		(1, user_channel_id, required),
		(2, former_temporary_channel_id, required),
		(3, counterparty_node_id, required),
		(4, funding_txo, required),
	},
	(5, ChannelClosed) => {
		(0, channel_id, required),
		(1, user_channel_id, required),
	};
);

pub struct EventQueue<K: Deref, L: Deref>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	queue: Mutex<VecDeque<Event>>,
	notifier: Condvar,
	kv_store: K,
	logger: L,
}

impl<K: Deref, L: Deref> EventQueue<K, L>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	pub(crate) fn new(kv_store: K, logger: L) -> Self {
		let queue: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());
		let notifier = Condvar::new();
		Self { queue, notifier, kv_store, logger }
	}

	pub(crate) fn add_event(&self, event: Event) -> Result<(), Error> {
		{
			let mut locked_queue = self.queue.lock().unwrap();
			locked_queue.push_back(event);
			self.write_queue_and_commit(&locked_queue)?;
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
			self.write_queue_and_commit(&locked_queue)?;
		}
		self.notifier.notify_one();
		Ok(())
	}

	fn write_queue_and_commit(&self, locked_queue: &VecDeque<Event>) -> Result<(), Error> {
		let mut writer = self
			.kv_store
			.write(EVENT_QUEUE_PERSISTENCE_NAMESPACE, EVENT_QUEUE_PERSISTENCE_KEY)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Getting writer for key {}/{} failed due to: {}",
					EVENT_QUEUE_PERSISTENCE_NAMESPACE,
					EVENT_QUEUE_PERSISTENCE_KEY,
					e
				);
				Error::PersistenceFailed
			})?;
		EventQueueSerWrapper(locked_queue).write(&mut writer).map_err(|e| {
			log_error!(
				self.logger,
				"Writing event queue data to key {}/{} failed due to: {}",
				EVENT_QUEUE_PERSISTENCE_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_KEY,
				e
			);
			Error::PersistenceFailed
		})?;
		writer.commit().map_err(|e| {
			log_error!(
				self.logger,
				"Committing event queue data to key {}/{} failed due to: {}",
				EVENT_QUEUE_PERSISTENCE_NAMESPACE,
				EVENT_QUEUE_PERSISTENCE_KEY,
				e
			);
			Error::PersistenceFailed
		})?;
		Ok(())
	}
}

impl<K: Deref, L: Deref> ReadableArgs<(K, L)> for EventQueue<K, L>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, args: (K, L),
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let (kv_store, logger) = args;
		let read_queue: EventQueueDeserWrapper = Readable::read(reader)?;
		let queue: Mutex<VecDeque<Event>> = Mutex::new(read_queue.0);
		let notifier = Condvar::new();
		Ok(Self { queue, notifier, kv_store, logger })
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
	K::Target: KVStore,
	L::Target: Logger,
{
	wallet: Arc<Wallet<bdk::database::SqliteDatabase>>,
	event_queue: Arc<EventQueue<K, L>>,
	channel_manager: Arc<ChannelManager>,
	network_graph: Arc<NetworkGraph>,
	keys_manager: Arc<KeysManager>,
	payment_store: Arc<PaymentStore<K, L>>,
	runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>,
	logger: L,
	_config: Arc<Config>,
}

impl<K: Deref + Clone, L: Deref> EventHandler<K, L>
where
	K::Target: KVStore,
	L::Target: Logger,
{
	pub fn new(
		wallet: Arc<Wallet<bdk::database::SqliteDatabase>>, event_queue: Arc<EventQueue<K, L>>,
		channel_manager: Arc<ChannelManager>, network_graph: Arc<NetworkGraph>,
		keys_manager: Arc<KeysManager>, payment_store: Arc<PaymentStore<K, L>>,
		runtime: Arc<RwLock<Option<tokio::runtime::Runtime>>>, logger: L, _config: Arc<Config>,
	) -> Self {
		Self {
			event_queue,
			wallet,
			channel_manager,
			network_graph,
			keys_manager,
			payment_store,
			logger,
			runtime,
			_config,
		}
	}

	pub async fn handle_event(&self, event: LdkEvent) {
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
				claim_deadline: _,
				onion_fields: _,
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

						let update = PaymentDetailsUpdate {
							status: Some(PaymentStatus::Failed),
							..PaymentDetailsUpdate::new(payment_hash)
						};
						self.payment_store.update(&update).expect("Failed to access payment store");
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

					let update = PaymentDetailsUpdate {
						status: Some(PaymentStatus::Failed),
						..PaymentDetailsUpdate::new(payment_hash)
					};
					self.payment_store.update(&update).expect("Failed to access payment store");
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
				match purpose {
					PaymentPurpose::InvoicePayment { payment_preimage, payment_secret, .. } => {
						let update = PaymentDetailsUpdate {
							preimage: Some(payment_preimage),
							secret: Some(Some(payment_secret)),
							amount_msat: Some(Some(amount_msat)),
							status: Some(PaymentStatus::Succeeded),
							..PaymentDetailsUpdate::new(payment_hash)
						};
						match self.payment_store.update(&update) {
							Ok(true) => (),
							Ok(false) => {
								log_error!(
									self.logger,
									"Payment with hash {} couldn't be found in store",
									hex_utils::to_string(&payment_hash.0)
								);
								debug_assert!(false);
							}
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to update payment with hash {}: {}",
									hex_utils::to_string(&payment_hash.0),
									e
								);
								debug_assert!(false);
							}
						}
					}
					PaymentPurpose::SpontaneousPayment(preimage) => {
						let payment = PaymentDetails {
							preimage: Some(preimage),
							hash: payment_hash,
							secret: None,
							amount_msat: Some(amount_msat),
							direction: PaymentDirection::Inbound,
							status: PaymentStatus::Succeeded,
						};

						match self.payment_store.insert(payment) {
							Ok(false) => (),
							Ok(true) => {
								log_error!(
									self.logger,
									"Spontaneous payment with hash {} was previosly known",
									hex_utils::to_string(&payment_hash.0)
								);
								debug_assert!(false);
							}
							Err(e) => {
								log_error!(
									self.logger,
									"Failed to insert payment with hash {}: {}",
									hex_utils::to_string(&payment_hash.0),
									e
								);
								debug_assert!(false);
							}
						}
					}
				};

				self.event_queue
					.add_event(Event::PaymentReceived { payment_hash, amount_msat })
					.expect("Failed to push to event queue");
			}
			LdkEvent::PaymentSent { payment_preimage, payment_hash, fee_paid_msat, .. } => {
				if let Some(mut payment) = self.payment_store.get(&payment_hash) {
					payment.preimage = Some(payment_preimage);
					payment.status = PaymentStatus::Succeeded;
					self.payment_store
						.insert(payment.clone())
						.expect("Failed to access payment store");
					log_info!(
						self.logger,
						"Successfully sent payment of {}msat{} from \
						payment hash {:?} with preimage {:?}",
						payment.amount_msat.unwrap(),
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

				let update = PaymentDetailsUpdate {
					status: Some(PaymentStatus::Failed),
					..PaymentDetailsUpdate::new(payment_hash)
				};
				self.payment_store.update(&update).expect("Failed to access payment store");
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

				let runtime_lock = self.runtime.read().unwrap();
				debug_assert!(runtime_lock.is_some());

				if let Some(runtime) = runtime_lock.as_ref() {
					runtime.spawn(async move {
						let millis_to_sleep = thread_rng().gen_range(min..min * 5) as u64;
						tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;

						forwarding_channel_manager.process_pending_htlc_forwards();
					});
				}
			}
			LdkEvent::SpendableOutputs { outputs } => {
				// TODO: We should eventually remember the outputs and supply them to the wallet's coin selection, once BDK allows us to do so.
				let destination_address =
					self.wallet.get_new_address().expect("Failed to get destination address");
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
				outbound_amount_forwarded_msat,
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
				let outbound_amount_forwarded_msat = outbound_amount_forwarded_msat.unwrap_or(0);
				if claim_from_onchain_tx {
					log_info!(
						self.logger,
						"Forwarded payment{}{} of {}msat, earning {}msat in fees from claiming onchain.",
						from_prev_str,
						to_next_str,
						outbound_amount_forwarded_msat,
						fee_earned,
					);
				} else {
					log_info!(
						self.logger,
						"Forwarded payment{}{} of {}msat, earning {}msat in fees.",
						from_prev_str,
						to_next_str,
						outbound_amount_forwarded_msat,
						fee_earned,
					);
				}
			}
			LdkEvent::ChannelPending {
				channel_id,
				user_channel_id,
				former_temporary_channel_id,
				counterparty_node_id,
				funding_txo,
			} => {
				log_info!(
					self.logger,
					"New channel {} with counterparty {} has been created and is pending confirmation on chain.",
					hex_utils::to_string(&channel_id),
					counterparty_node_id,
				);
				self.event_queue
					.add_event(Event::ChannelPending {
						channel_id: ChannelId(channel_id),
						user_channel_id: UserChannelId(user_channel_id),
						former_temporary_channel_id: ChannelId(
							former_temporary_channel_id.unwrap(),
						),
						counterparty_node_id,
						funding_txo,
					})
					.expect("Failed to push to event queue");
			}
			LdkEvent::ChannelReady {
				channel_id, user_channel_id, counterparty_node_id, ..
			} => {
				log_info!(
					self.logger,
					"Channel {} with counterparty {} ready to be used.",
					hex_utils::to_string(&channel_id),
					counterparty_node_id,
				);
				self.event_queue
					.add_event(Event::ChannelReady {
						channel_id: ChannelId(channel_id),
						user_channel_id: UserChannelId(user_channel_id),
					})
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
					.add_event(Event::ChannelClosed {
						channel_id: ChannelId(channel_id),
						user_channel_id: UserChannelId(user_channel_id),
					})
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
	use crate::test::utils::{TestLogger, TestStore};

	#[test]
	fn event_queue_persistence() {
		let store = Arc::new(TestStore::new());
		let logger = Arc::new(TestLogger::new());
		let event_queue = EventQueue::new(Arc::clone(&store), Arc::clone(&logger));

		let expected_event = Event::ChannelReady {
			channel_id: ChannelId([23u8; 32]),
			user_channel_id: UserChannelId(2323),
		};
		event_queue.add_event(expected_event.clone()).unwrap();
		assert!(store.get_and_clear_did_persist());

		// Check we get the expected event and that it is returned until we mark it handled.
		for _ in 0..5 {
			assert_eq!(event_queue.next_event(), expected_event);
			assert_eq!(false, store.get_and_clear_did_persist());
		}

		// Check we can read back what we persisted.
		let persisted_bytes = store
			.get_persisted_bytes(EVENT_QUEUE_PERSISTENCE_NAMESPACE, EVENT_QUEUE_PERSISTENCE_KEY)
			.unwrap();
		let deser_event_queue =
			EventQueue::read(&mut &persisted_bytes[..], (Arc::clone(&store), logger)).unwrap();
		assert_eq!(deser_event_queue.next_event(), expected_event);
		assert!(!store.get_and_clear_did_persist());

		// Check we persisted on `event_handled()`
		event_queue.event_handled().unwrap();

		assert!(store.get_and_clear_did_persist());
	}
}
