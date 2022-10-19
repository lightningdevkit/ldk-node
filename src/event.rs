use crate::{
	hex_utils, ChannelManager, Error, FilesystemPersister, LdkLiteChainAccess, LdkLiteConfig,
	NetworkGraph, PaymentInfo, PaymentInfoStorage, PaymentStatus,
};

#[allow(unused_imports)]
use crate::logger::{
	log_error, log_given_level, log_info, log_internal, log_trace, log_warn, FilesystemLogger,
	Logger,
};

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::keysinterface::KeysManager;
use lightning::ln::PaymentHash;
use lightning::routing::gossip::NodeId;
use lightning::util::events::Event as LdkEvent;
use lightning::util::events::EventHandler as LdkEventHandler;
use lightning::util::events::PaymentPurpose;
use lightning::util::persist::KVStorePersister;
use lightning::util::ser::{Readable, ReadableArgs, Writeable, Writer};

use bitcoin::secp256k1::Secp256k1;
use rand::{thread_rng, Rng};
use std::collections::{hash_map, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// The event queue will be persisted under this key.
pub(crate) const EVENTS_PERSISTENCE_KEY: &str = "events";

/// An event emitted by [`LdkLite`] that should be handled by the user.
///
/// [`LdkLite`]: [`crate::LdkLite`]
#[derive(Debug, Clone)]
pub enum LdkLiteEvent {
	/// A payment we sent was successful.
	PaymentSuccessful {
		/// The hash of the payment.
		payment_hash: PaymentHash,
	},
	/// A payment we sent has failed.
	PaymentFailed {
		/// The hash of the payment.
		payment_hash: PaymentHash,
	},
	/// A payment has been received.
	PaymentReceived {
		/// The hash of the payment.
		payment_hash: PaymentHash,
		/// The value, in thousandths of a satoshi that has been received.
		amount_msat: u64,
	},
	// TODO: Implement after a corresponding LDK event is added.
	//ChannelOpened {
	//},
	/// A channel has been closed.
	ChannelClosed {
		/// The channel_id of the channel which has been closed.
		channel_id: [u8; 32],
	},
	// TODO: Implement on-chain events when better integrating with BDK wallet sync.
	//OnChainPaymentSent {
	//},
	//OnChainPaymentReceived {
	//}
}

impl Readable for LdkLiteEvent {
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
			//3u8 => {
			// TODO ChannelOpened
			//}
			4u8 => {
				let channel_id: [u8; 32] = Readable::read(reader)?;
				Ok(Self::ChannelClosed { channel_id })
			}
			//5u8 => {
			// TODO OnChainPaymentSent
			//}
			//6u8 => {
			// TODO OnChainPaymentReceived
			//}
			_ => Err(lightning::ln::msgs::DecodeError::InvalidValue),
		}
	}
}

impl Writeable for LdkLiteEvent {
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
			//Self::ChannelOpened { .. } => {
			//TODO
			//}
			Self::ChannelClosed { channel_id } => {
				4u8.write(writer)?;
				channel_id.write(writer)?;
				Ok(())
			} //Self::OnChainPaymentSent { .. } => {
			  //TODO
			  //}
			  //Self::OnChainPaymentReceived { .. } => {
			  //TODO
			  //}
		}
	}
}

pub(crate) struct LdkLiteEventQueue<K: KVStorePersister> {
	queue: Mutex<EventQueueSerWrapper>,
	notifier: Condvar,
	persister: Arc<K>,
}

impl<K: KVStorePersister> LdkLiteEventQueue<K> {
	pub(crate) fn new(persister: Arc<K>) -> Self {
		let queue: Mutex<EventQueueSerWrapper> = Mutex::new(EventQueueSerWrapper(VecDeque::new()));
		let notifier = Condvar::new();
		Self { queue, notifier, persister }
	}
	pub(crate) fn add_event(&self, event: LdkLiteEvent) -> Result<(), Error> {
		let mut locked_queue = self.queue.lock().unwrap();
		locked_queue.0.push_back(event);

		self.persister
			.persist(EVENTS_PERSISTENCE_KEY, &*locked_queue)
			.map_err(|_| Error::PersistenceFailed)?;

		self.notifier.notify_one();
		Ok(())
	}

	pub(crate) fn next_event(&self) -> LdkLiteEvent {
		let locked_queue = self
			.notifier
			.wait_while(self.queue.lock().unwrap(), |queue| queue.0.is_empty())
			.unwrap();
		locked_queue.0.front().unwrap().clone()
	}

	pub(crate) fn event_handled(&self) -> Result<(), Error> {
		let mut locked_queue = self.queue.lock().unwrap();
		locked_queue.0.pop_front();
		self.persister
			.persist(EVENTS_PERSISTENCE_KEY, &*locked_queue)
			.map_err(|_| Error::PersistenceFailed)?;
		self.notifier.notify_one();
		Ok(())
	}
}

impl<K: KVStorePersister> ReadableArgs<Arc<K>> for LdkLiteEventQueue<K> {
	#[inline]
	fn read<R: lightning::io::Read>(
		reader: &mut R, persister: Arc<K>,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let queue: Mutex<EventQueueSerWrapper> = Mutex::new(Readable::read(reader)?);
		let notifier = Condvar::new();
		Ok(Self { queue, notifier, persister })
	}
}

struct EventQueueSerWrapper(VecDeque<LdkLiteEvent>);

impl Readable for EventQueueSerWrapper {
	fn read<R: lightning::io::Read>(
		reader: &mut R,
	) -> Result<Self, lightning::ln::msgs::DecodeError> {
		let len: u16 = Readable::read(reader)?;
		let mut queue = VecDeque::with_capacity(len as usize);
		for _ in 0..len {
			queue.push_back(Readable::read(reader)?);
		}
		Ok(EventQueueSerWrapper(queue))
	}
}

impl Writeable for EventQueueSerWrapper {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), lightning::io::Error> {
		(self.0.len() as u16).write(writer)?;
		for e in self.0.iter() {
			e.write(writer)?;
		}
		Ok(())
	}
}

pub(crate) struct LdkLiteEventHandler {
	chain_access: Arc<LdkLiteChainAccess<bdk::sled::Tree>>,
	event_queue: Arc<LdkLiteEventQueue<FilesystemPersister>>,
	channel_manager: Arc<ChannelManager>,
	network_graph: Arc<NetworkGraph>,
	keys_manager: Arc<KeysManager>,
	inbound_payments: Arc<PaymentInfoStorage>,
	outbound_payments: Arc<PaymentInfoStorage>,
	logger: Arc<FilesystemLogger>,
	_config: Arc<LdkLiteConfig>,
}

impl LdkLiteEventHandler {
	pub fn new(
		chain_access: Arc<LdkLiteChainAccess<bdk::sled::Tree>>,
		event_queue: Arc<LdkLiteEventQueue<FilesystemPersister>>,
		channel_manager: Arc<ChannelManager>, network_graph: Arc<NetworkGraph>,
		keys_manager: Arc<KeysManager>, inbound_payments: Arc<PaymentInfoStorage>,
		outbound_payments: Arc<PaymentInfoStorage>, logger: Arc<FilesystemLogger>,
		_config: Arc<LdkLiteConfig>,
	) -> Self {
		Self {
			event_queue,
			chain_access,
			channel_manager,
			network_graph,
			keys_manager,
			inbound_payments,
			outbound_payments,
			logger,
			_config,
		}
	}
}

impl LdkEventHandler for LdkLiteEventHandler {
	fn handle_event(&self, event: &LdkEvent) {
		match event {
			LdkEvent::FundingGenerationReady {
				temporary_channel_id,
				counterparty_node_id,
				channel_value_satoshis,
				output_script,
				..
			} => {
				// Construct the raw transaction with one output, that is paid the amount of the
				// channel.
				let confirmation_target = ConfirmationTarget::Normal;

				// Sign the final funding transaction and broadcast it.
				match self.chain_access.create_funding_transaction(
					&output_script,
					*channel_value_satoshis,
					confirmation_target,
				) {
					Ok(final_tx) => {
						// Give the funding transaction back to LDK for opening the channel.
						if self
							.channel_manager
							.funding_transaction_generated(
								&temporary_channel_id,
								counterparty_node_id,
								final_tx,
							)
							.is_err()
						{
							log_error!(self.logger, "Channel went away before we could fund it. The peer disconnected or refused the channel");
						}
					}
					Err(err) => {
						log_error!(self.logger, "Failed to create funding transaction: {}", err);
					}
				}
			}
			LdkEvent::PaymentReceived { payment_hash, purpose, amount_msat } => {
				log_info!(
					self.logger,
					"Received payment from payment hash {} of {} millisatoshis",
					hex_utils::to_string(&payment_hash.0),
					amount_msat,
				);
				let payment_preimage = match purpose {
					PaymentPurpose::InvoicePayment { payment_preimage, .. } => {
						*payment_preimage
					}
					PaymentPurpose::SpontaneousPayment(preimage) => Some(*preimage),
				};
				self.channel_manager.claim_funds(payment_preimage.unwrap());
				self.event_queue
					.add_event(LdkLiteEvent::PaymentReceived {
						payment_hash: *payment_hash,
						amount_msat: *amount_msat,
					})
					.unwrap();
			}
			LdkEvent::PaymentClaimed { payment_hash, purpose, amount_msat } => {
				log_info!(
					self.logger,
					"Claimed payment from payment hash {} of {} millisatoshis",
					hex_utils::to_string(&payment_hash.0),
					amount_msat,
				);
				let (payment_preimage, payment_secret) = match purpose {
					PaymentPurpose::InvoicePayment {
						payment_preimage,
						payment_secret,
						..
					} => (*payment_preimage, Some(*payment_secret)),
					PaymentPurpose::SpontaneousPayment(preimage) => {
						(Some(*preimage), None)
					}
				};
				let mut payments = self.inbound_payments.lock().unwrap();
				match payments.entry(*payment_hash) {
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
							amount_msat: Some(*amount_msat),
						});
					}
				}
			}
			LdkEvent::PaymentSent {
				payment_preimage,
				payment_hash,
				fee_paid_msat,
				..
			} => {
				let mut payments = self.outbound_payments.lock().unwrap();
				for (hash, payment) in payments.iter_mut() {
					if *hash == *payment_hash {
						payment.preimage = Some(*payment_preimage);
						payment.status = PaymentStatus::Succeeded;
						log_info!(
							self.logger,
							"Successfully sent payment of {} millisatoshis{} from \
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
				}
				self.event_queue
					.add_event(LdkLiteEvent::PaymentSuccessful {
						payment_hash: *payment_hash,
					})
					.unwrap();
			}
			LdkEvent::PaymentFailed { payment_hash, .. } => {
				log_info!(
					self.logger,
					"Failed to send payment to payment hash {:?}: exhausted payment retry attempts",
					hex_utils::to_string(&payment_hash.0)
				);

				let mut payments = self.outbound_payments.lock().unwrap();
				if payments.contains_key(&payment_hash) {
					let payment = payments.get_mut(&payment_hash).unwrap();
					payment.status = PaymentStatus::Failed;
				}
				self.event_queue
					.add_event(LdkLiteEvent::PaymentFailed {
						payment_hash: *payment_hash,
					})
					.unwrap();
			}

			LdkEvent::PaymentPathSuccessful { .. } => {}
			LdkEvent::PaymentPathFailed { .. } => {}
			LdkEvent::ProbeSuccessful { .. } => {}
			LdkEvent::ProbeFailed { .. } => {}
			LdkEvent::HTLCHandlingFailed { .. } => {}
			LdkEvent::PendingHTLCsForwardable { time_forwardable } => {
				let forwarding_channel_manager = self.channel_manager.clone();
				let min = time_forwardable.as_millis() as u64;

				// TODO: any way we still can use tokio here?
				// TODO: stop this thread on shutdown
				thread::spawn(move || {
					let millis_to_sleep = thread_rng().gen_range(min..min * 5) as u64;
					thread::sleep(Duration::from_millis(millis_to_sleep));
					forwarding_channel_manager.process_pending_htlc_forwards();
				});
			}
			LdkEvent::SpendableOutputs { outputs } => {
				let destination_address = self.chain_access.get_new_address().unwrap();
				let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
				let tx_feerate =
					self.chain_access.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
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
				self.chain_access.broadcast_transaction(&spending_tx);
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

				let node_str = |channel_id: &Option<[u8; 32]>| match channel_id {
					None => String::new(),
					Some(channel_id) => match channels.iter().find(|c| c.channel_id == *channel_id)
					{
						None => String::new(),
						Some(channel) => {
							match nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id)) {
								None => "private node".to_string(),
								Some(node) => match &node.announcement_info {
									None => "unnamed node".to_string(),
									Some(announcement) => {
										format!("node {}", announcement.alias)
									}
								},
							}
						}
					},
				};
				let channel_str = |channel_id: &Option<[u8; 32]>| {
					channel_id
						.map(|channel_id| {
							format!(" with channel {}", hex_utils::to_string(&channel_id))
						})
						.unwrap_or_default()
				};
				let from_prev_str =
					format!(" from {}{}", node_str(prev_channel_id), channel_str(prev_channel_id));
				let to_next_str =
					format!(" to {}{}", node_str(next_channel_id), channel_str(next_channel_id));

				let from_onchain_str = if *claim_from_onchain_tx {
					"from onchain downstream claim"
				} else {
					"from HTLC fulfill message"
				};
				if let Some(fee_earned) = fee_earned_msat {
					log_info!(
						self.logger,
						"Forwarded payment{}{}, earning {} msat {}",
						from_prev_str,
						to_next_str,
						fee_earned,
						from_onchain_str
					);
				} else {
					log_info!(
						self.logger,
						"Forwarded payment{}{}, claiming onchain {}",
						from_prev_str,
						to_next_str,
						from_onchain_str
					);
				}
			}

			LdkEvent::ChannelClosed { channel_id, reason, user_channel_id: _ } => {
				log_info!(
					self.logger,
					"Channel {} closed due to: {:?}",
					hex_utils::to_string(channel_id),
					reason
				);
				self.event_queue
					.add_event(LdkLiteEvent::ChannelClosed {
						channel_id: *channel_id,
					})
					.unwrap();
			}
			LdkEvent::DiscardFunding { .. } => {}
		}
	}
}
