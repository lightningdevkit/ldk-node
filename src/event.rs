use crate::{
	hex_utils, ChannelManager, Config, Error, KeysManager, NetworkGraph, PaymentInfo,
	PaymentInfoStorage, PaymentStatus, Wallet,
};

use crate::logger::{log_error, log_given_level, log_info, log_internal, Logger};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
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
use std::collections::{hash_map, VecDeque};
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The event queue will be persisted under this key.
pub(crate) const EVENTS_PERSISTENCE_KEY: &str = "events";

/// An event emitted by [`LdkLite`], which should be handled by the user.
///
/// [`LdkLite`]: [`crate::LdkLite`]
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

// TODO: Figure out serialization more concretely - see issue #30
impl Readable for Event {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		match Readable::read(reader)? {
			0u8 => {
				let payment_hash: PaymentHash = Readable::read(reader)?;
				Ok(Self::PaymentSuccessful { payment_hash })
			}
			1u8 => {
				let payment_hash: PaymentHash = Readable::read(reader)?;
				Ok(Self::PaymentFailed { payment_hash })
			}
			2u8 => {
				let payment_hash: PaymentHash = Readable::read(reader)?;
				let amount_msat: u64 = Readable::read(reader)?;
				Ok(Self::PaymentReceived { payment_hash, amount_msat })
			}
			3u8 => {
				let channel_id: [u8; 32] = Readable::read(reader)?;
				let user_channel_id: u128 = Readable::read(reader)?;
				Ok(Self::ChannelReady { channel_id, user_channel_id })
			}
			4u8 => {
				let channel_id: [u8; 32] = Readable::read(reader)?;
				let user_channel_id: u128 = Readable::read(reader)?;
				Ok(Self::ChannelClosed { channel_id, user_channel_id })
			}
			_ => Err(lightning::ln::msgs::DecodeError::InvalidValue),
		}
	}
}

impl Writeable for Event {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		match self {
			Self::PaymentSuccessful { payment_hash } => {
				0u8.write(writer)?;
				payment_hash.write(writer)?;
				Ok(())
			}
			Self::PaymentFailed { payment_hash } => {
				1u8.write(writer)?;
				payment_hash.write(writer)?;
				Ok(())
			}
			Self::PaymentReceived { payment_hash, amount_msat } => {
				2u8.write(writer)?;
				payment_hash.write(writer)?;
				amount_msat.write(writer)?;
				Ok(())
			}
			Self::ChannelReady { channel_id, user_channel_id } => {
				3u8.write(writer)?;
				channel_id.write(writer)?;
				user_channel_id.write(writer)?;
				Ok(())
			}
			Self::ChannelClosed { channel_id, user_channel_id } => {
				4u8.write(writer)?;
				channel_id.write(writer)?;
				user_channel_id.write(writer)?;
				Ok(())
			}
		}
	}
}

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

pub(crate) struct EventHandler<K: Deref, L: Deref>
where
	K::Target: KVStorePersister,
	L::Target: Logger,
{
	wallet: Arc<Wallet<bdk::sled::Tree>>,
	event_queue: Arc<EventQueue<K>>,
	channel_manager: Arc<ChannelManager>,
	network_graph: Arc<NetworkGraph>,
	keys_manager: Arc<KeysManager>,
	inbound_payments: Arc<PaymentInfoStorage>,
	outbound_payments: Arc<PaymentInfoStorage>,
	tokio_runtime: Arc<tokio::runtime::Runtime>,
	logger: L,
	_config: Arc<Config>,
}

impl<K: Deref, L: Deref> EventHandler<K, L>
where
	K::Target: KVStorePersister,
	L::Target: Logger,
{
	pub fn new(
		wallet: Arc<Wallet<bdk::sled::Tree>>, event_queue: Arc<EventQueue<K>>,
		channel_manager: Arc<ChannelManager>, network_graph: Arc<NetworkGraph>,
		keys_manager: Arc<KeysManager>, inbound_payments: Arc<PaymentInfoStorage>,
		outbound_payments: Arc<PaymentInfoStorage>, tokio_runtime: Arc<tokio::runtime::Runtime>,
		logger: L, _config: Arc<Config>,
	) -> Self {
		Self {
			event_queue,
			wallet,
			channel_manager,
			network_graph,
			keys_manager,
			inbound_payments,
			outbound_payments,
			logger,
			tokio_runtime,
			_config,
		}
	}
}

impl<K: Deref, L: Deref> LdkEventHandler for EventHandler<K, L>
where
	K::Target: KVStorePersister,
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
					&output_script,
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
					Err(_err) => {
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
				log_info!(
					self.logger,
					"Received payment from payment hash {} of {} msats",
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
					self.inbound_payments.lock().unwrap().remove(&payment_hash);
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
					"Claimed payment from payment hash {} of {} msats.",
					hex_utils::to_string(&payment_hash.0),
					amount_msat,
				);
				let (payment_preimage, payment_secret) = match purpose {
					PaymentPurpose::InvoicePayment { payment_preimage, payment_secret, .. } => {
						(payment_preimage, Some(payment_secret))
					}
					PaymentPurpose::SpontaneousPayment(preimage) => (Some(preimage), None),
				};
				let mut payments = self.inbound_payments.lock().unwrap();
				match payments.entry(payment_hash) {
					hash_map::Entry::Occupied(mut e) => {
						let payment = e.get_mut();
						payment.status = PaymentStatus::Succeeded;
						payment.preimage = payment_preimage;
						payment.secret = payment_secret;
					}
					hash_map::Entry::Vacant(e) => {
						e.insert(PaymentInfo {
							preimage: payment_preimage,
							secret: payment_secret,
							status: PaymentStatus::Succeeded,
							amount_msat: Some(amount_msat),
						});
					}
				}
				self.event_queue
					.add_event(Event::PaymentReceived { payment_hash, amount_msat })
					.expect("Failed to push to event queue");
			}
			LdkEvent::PaymentSent { payment_preimage, payment_hash, fee_paid_msat, .. } => {
				let mut payments = self.outbound_payments.lock().unwrap();
				for (hash, payment) in payments.iter_mut() {
					if *hash == payment_hash {
						payment.preimage = Some(payment_preimage);
						payment.status = PaymentStatus::Succeeded;
						log_info!(
							self.logger,
							"Successfully sent payment of {} msats{} from \
								 payment hash {:?} with preimage {:?}",
							payment.amount_msat.unwrap(),
							if let Some(fee) = fee_paid_msat {
								format!(" (fee {} msats)", fee)
							} else {
								"".to_string()
							},
							hex_utils::to_string(&payment_hash.0),
							hex_utils::to_string(&payment_preimage.0)
						);
						break;
					}
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

				let mut payments = self.outbound_payments.lock().unwrap();
				if payments.contains_key(&payment_hash) {
					let payment = payments.get_mut(&payment_hash).unwrap();
					assert_eq!(payment.status, PaymentStatus::Pending);
					payment.status = PaymentStatus::Failed;
				}
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
				let spending_tx = self
					.keys_manager
					.spend_spendable_outputs(
						output_descriptors,
						Vec::new(),
						destination_address.script_pubkey(),
						tx_feerate,
						&Secp256k1::new(),
					)
					.unwrap();
				self.wallet.broadcast_transaction(&spending_tx);
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
						"Forwarded payment{}{}, earning {} msats in fees from claiming onchain.",
						from_prev_str,
						to_next_str,
						fee_earned,
					);
				} else {
					log_info!(
						self.logger,
						"Forwarded payment{}{}, earning {} msats in fees.",
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
	use crate::tests::test_utils::TestPersister;

	#[test]
	fn event_queue_persistence() {
		let test_persister = Arc::new(TestPersister::new());
		let event_queue = EventQueue::new(Arc::clone(&test_persister));

		let expected_event = Event::ChannelReady { channel_id: [23u8; 32], user_channel_id: 2323 };
		event_queue.add_event(expected_event.clone()).unwrap();
		assert!(test_persister.get_and_clear_pending_persist());

		// Check we get the expected event and that it is returned until we mark it handled.
		for _ in 0..5 {
			assert_eq!(event_queue.next_event(), expected_event);
			assert_eq!(false, test_persister.get_and_clear_pending_persist());
		}

		// Check we persisted on `event_handled()`
		event_queue.event_handled().unwrap();
		assert!(test_persister.get_and_clear_pending_persist());
	}
}
