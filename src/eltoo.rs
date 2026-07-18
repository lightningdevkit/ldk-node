// This file is Copyright its authors and licensed under the Apache License, Version 2.0 or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option. You may not use this
// file except in accordance with one or both of these licenses.

//! Experimental support for eltoo (LN-Symmetry over BIP-448) channels.
//!
//! This wires the eltoo PoC stack of our rust-lightning fork into the node:
//! [`EltooChannelManager`] runs alongside the regular `ChannelManager`, sharing the
//! peer transport (via [`DualChannelMessageHandler`]), the BDK on-chain wallet
//! (funding + P2A anchor CPFP fee-bumping), the bitcoind chain source (block feed)
//! and the transaction broadcaster. Only the bitcoind chain source feeds eltoo
//! channels; esplora/electrum do not.
//!
//! Scope mirrors the PoC: no persistence (channels do not survive restarts), no
//! reorg handling, no routed payments (direct channels only, with out-of-band
//! payment hashes standing in for invoices).

use crate::logger::{log_debug, log_error, LdkLogger, Logger};
use crate::types::{Broadcaster, ChannelManager, KeysManager, Wallet};

use lightning::chain::chaininterface::{BroadcasterInterface, TransactionType};
use lightning::chain::{BlockLocator, Listen};
use lightning::ln::eltoo::channel::EltooChannelConfig;
use lightning::ln::eltoo::channelmanager::{EltooChannelManager, EltooEvent};
use lightning::ln::eltoo::peer_glue::EltooMessageHandler;
use lightning::ln::msgs;
use lightning::ln::msgs::{BaseMessageHandler, ChannelMessageHandler, MessageSendEvent};
use lightning::ln::types::ChannelId;
use lightning::types::features::{InitFeatures, NodeFeatures};
use lightning::util::wallet_utils::WalletSource;

use bitcoin::absolute::LockTime;
use bitcoin::constants::ChainHash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::transaction::Version;
use bitcoin::{
	Amount, OutPoint, Psbt, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) type EltooManager = EltooChannelManager<Arc<KeysManager>>;
pub(crate) type EltooHandler = EltooMessageHandler<Arc<KeysManager>>;

/// Fee reserved for a P2A anchor CPFP child (PoC: fixed, no estimation).
const ANCHOR_CHILD_FEE: Amount = Amount::from_sat(20_000);

/// An eltoo channel we opened, between channel creation and `channel_ready`.
struct PendingFunding {
	/// The channel id — temporary until the funding tx is created, final afterwards.
	channel_id: ChannelId,
	capacity: Amount,
	funding_txid: Option<Txid>,
	ready_signalled: bool,
}

/// Owns the node-side glue around the sans-IO [`EltooChannelManager`]: the funding
/// state machine, the broadcast/CPFP pump and the chain feed. Driven by a periodic
/// background task (see [`EltooRuntime::process`]).
pub(crate) struct EltooRuntime {
	pub(crate) manager: Arc<EltooManager>,
	pub(crate) handler: Arc<EltooHandler>,
	config: EltooChannelConfig,
	network: bitcoin::Network,
	wallet: Arc<Wallet>,
	tx_broadcaster: Arc<Broadcaster>,
	logger: Arc<Logger>,
	pending_funding: Mutex<Vec<PendingFunding>>,
	/// Watched funding txids → confirmation height (once seen in a block).
	confirmations: Mutex<HashMap<Txid, u32>>,
	tip: AtomicU32,
	/// Non-broadcast events, drained by the user via `Node::eltoo_events`.
	events: Mutex<Vec<EltooEvent>>,
}

impl EltooRuntime {
	pub(crate) fn new(
		node_id: PublicKey, keys_manager: Arc<KeysManager>, network: bitcoin::Network,
		best_height: u32, wallet: Arc<Wallet>, tx_broadcaster: Arc<Broadcaster>,
		logger: Arc<Logger>,
	) -> Self {
		let chain_hash = ChainHash::using_genesis_block(network);
		let config = EltooChannelConfig::default();
		let manager = Arc::new(EltooChannelManager::new(
			keys_manager,
			node_id,
			config,
			chain_hash,
			best_height,
		));
		let handler = Arc::new(EltooMessageHandler::new(Arc::clone(&manager), chain_hash));
		Self {
			manager,
			handler,
			config,
			network,
			wallet,
			tx_broadcaster,
			logger,
			pending_funding: Mutex::new(Vec::new()),
			confirmations: Mutex::new(HashMap::new()),
			tip: AtomicU32::new(best_height),
			events: Mutex::new(Vec::new()),
		}
	}

	pub(crate) fn tip(&self) -> u32 {
		self.tip.load(Ordering::Acquire)
	}

	/// Registers a just-created channel; the periodic task funds it once the peer's
	/// accept has provided the funding address.
	pub(crate) fn register_pending_channel(&self, channel_id: ChannelId, capacity_sat: u64) {
		self.pending_funding.lock().unwrap().push(PendingFunding {
			channel_id,
			capacity: Amount::from_sat(capacity_sat),
			funding_txid: None,
			ready_signalled: false,
		});
	}

	/// Drains the events the user should see (payment outcomes, channels ready, ...).
	pub(crate) fn take_events(&self) -> Vec<EltooEvent> {
		core::mem::take(&mut *self.events.lock().unwrap())
	}

	/// One pass of the periodic driver: fund accepted channels, signal funding depth,
	/// pump broadcasts (attaching CPFP children to zero-fee TRUC parents).
	pub(crate) async fn process(&self) {
		self.fund_accepted_channels();
		self.signal_confirmed_fundings();
		self.pump_events().await;
	}

	/// For each pending channel whose handshake completed, build, broadcast and
	/// register the funding transaction from the BDK wallet.
	fn fund_accepted_channels(&self) {
		let to_fund: Vec<(ChannelId, Amount, bitcoin::Address)> = {
			let pending = self.pending_funding.lock().unwrap();
			pending
				.iter()
				.filter(|entry| entry.funding_txid.is_none())
				.filter_map(|entry| {
					self.manager
						.funding_address(entry.channel_id, self.network)
						.map(|address| (entry.channel_id, entry.capacity, address))
				})
				.collect()
		};
		for (temp_id, capacity, address) in to_fund {
			let locktime = LockTime::from_height(self.tip()).unwrap_or(LockTime::ZERO);
			let funding_tx = match self.wallet.create_funding_transaction(
				address.script_pubkey(),
				capacity,
				crate::fee_estimator::ConfirmationTarget::ChannelFunding,
				locktime,
			) {
				Ok(tx) => tx,
				Err(e) => {
					log_error!(self.logger, "Failed to create eltoo funding tx: {}", e);
					continue;
				},
			};
			let txid = funding_tx.compute_txid();
			let vout = funding_tx
				.output
				.iter()
				.position(|out| out.script_pubkey == address.script_pubkey())
				.expect("funding output present") as u16;
			let final_id = match self.manager.funding_ready(temp_id, txid, vout) {
				Ok(id) => id,
				Err(e) => {
					log_error!(self.logger, "eltoo funding_ready failed: {:?}", e);
					continue;
				},
			};
			self.tx_broadcaster.broadcast_transactions(&[(
				&funding_tx,
				TransactionType::Sweep { channels: vec![] },
			)]);
			self.confirmations.lock().unwrap().insert(txid, 0);
			let mut pending = self.pending_funding.lock().unwrap();
			if let Some(entry) = pending.iter_mut().find(|entry| entry.channel_id == temp_id) {
				entry.channel_id = final_id;
				entry.funding_txid = Some(txid);
			}
			log_debug!(self.logger, "Broadcast eltoo funding tx {} for channel {}", txid, final_id);
		}
	}

	/// Signals `funding_confirmed` once a funding tx has the configured depth.
	fn signal_confirmed_fundings(&self) {
		let tip = self.tip();
		let mut ready = Vec::new();
		{
			let confirmations = self.confirmations.lock().unwrap();
			let mut pending = self.pending_funding.lock().unwrap();
			for entry in pending.iter_mut() {
				if entry.ready_signalled {
					continue;
				}
				let txid = match entry.funding_txid {
					Some(txid) => txid,
					None => continue,
				};
				match confirmations.get(&txid) {
					Some(&height) if height > 0 => {
						let depth = tip + 1 - height;
						if depth >= self.config.minimum_depth {
							entry.ready_signalled = true;
							ready.push(entry.channel_id);
						}
					},
					_ => {},
				}
			}
		}
		for channel_id in ready {
			if let Err(e) = self.manager.funding_confirmed(channel_id) {
				log_error!(self.logger, "eltoo funding_confirmed failed: {:?}", e);
			}
		}
	}

	/// Drains manager events: broadcasts go out (zero-fee TRUC parents packaged with
	/// a wallet-funded CPFP child), everything else is queued for the user.
	async fn pump_events(&self) {
		for event in self.manager.get_and_clear_events() {
			match event {
				EltooEvent::BroadcastTransaction { tx, anchor_outpoint: None } => {
					self.tx_broadcaster.broadcast_transactions(&[(
						&tx,
						TransactionType::Sweep { channels: vec![] },
					)]);
				},
				EltooEvent::BroadcastTransaction { tx, anchor_outpoint: Some(anchor) } => {
					match self.build_anchor_child(&tx, anchor).await {
						Ok(child) => {
							self.tx_broadcaster.broadcast_transactions(&[
								(&tx, TransactionType::Sweep { channels: vec![] }),
								(&child, TransactionType::Sweep { channels: vec![] }),
							]);
						},
						Err(()) => {
							log_error!(
								self.logger,
								"Failed to build CPFP child for eltoo tx {}",
								tx.compute_txid()
							);
						},
					}
				},
				other => self.events.lock().unwrap().push(other),
			}
		}
	}

	/// Builds the CPFP child spending the P2A anchor (empty witness) plus one
	/// confirmed wallet UTXO for fees, change back to the wallet. TRUC requires the
	/// child to be v3 like its parent.
	async fn build_anchor_child(
		&self, parent: &Transaction, anchor: OutPoint,
	) -> Result<Transaction, ()> {
		debug_assert_eq!(anchor.txid, parent.compute_txid());
		let utxos = self.wallet.list_confirmed_utxos().await?;
		let utxo = utxos
			.into_iter()
			.find(|utxo| utxo.output.value > ANCHOR_CHILD_FEE * 2)
			.ok_or_else(|| {
				log_error!(self.logger, "No confirmed wallet UTXO available for eltoo CPFP");
			})?;
		let change_script = self.wallet.get_change_script().await?;
		let anchor_output = parent
			.output
			.get(anchor.vout as usize)
			.cloned()
			.ok_or_else(|| log_error!(self.logger, "Anchor outpoint missing from parent"))?;
		let unsigned = Transaction {
			version: Version::non_standard(3),
			lock_time: LockTime::ZERO,
			input: vec![
				TxIn {
					previous_output: anchor,
					script_sig: ScriptBuf::new(),
					sequence: Sequence::MAX,
					witness: Witness::new(),
				},
				TxIn {
					previous_output: utxo.outpoint,
					script_sig: ScriptBuf::new(),
					sequence: Sequence::MAX,
					witness: Witness::new(),
				},
			],
			output: vec![TxOut {
				value: utxo.output.value + anchor_output.value - ANCHOR_CHILD_FEE,
				script_pubkey: change_script,
			}],
		};
		let mut psbt = Psbt::from_unsigned_tx(unsigned)
			.map_err(|e| log_error!(self.logger, "Failed to build CPFP psbt: {}", e))?;
		// The wallet signs its own input; the P2A anchor input spends with an empty
		// witness and needs no signature.
		psbt.inputs[0].witness_utxo = Some(anchor_output);
		psbt.inputs[1].witness_utxo = Some(utxo.output.clone());
		self.wallet.sign_psbt(psbt).await
	}
}

/// Feeds the block-oriented chain source into the eltoo stack: confirmation heights
/// for the funding tracker, then the manager itself (which implements [`Listen`]).
impl Listen for EltooRuntime {
	fn filtered_block_connected(
		&self, header: &bitcoin::block::Header,
		txdata: &lightning::chain::transaction::TransactionData, height: u32,
	) {
		self.tip.store(height, Ordering::Release);
		{
			let mut confirmations = self.confirmations.lock().unwrap();
			for (_, tx) in txdata {
				let txid = tx.compute_txid();
				if let Some(entry) = confirmations.get_mut(&txid) {
					if *entry == 0 {
						*entry = height;
					}
				}
			}
		}
		self.manager.filtered_block_connected(header, txdata, height);
	}

	fn blocks_disconnected(&self, _fork_point_block: BlockLocator) {
		// Reorgs are unsupported in the eltoo PoC.
	}
}

/// The peer-facing channel message handler: LN-penalty messages go to the regular
/// [`ChannelManager`], eltoo messages (and the BOLT 2 HTLC updates, which eltoo
/// reuses — this node routes them by which stack knows the channel: the PoC only
/// ever has them on eltoo channels when the regular manager doesn't know them) go to
/// the [`EltooHandler`].
pub(crate) struct DualChannelMessageHandler {
	pub(crate) ldk: Arc<ChannelManager>,
	pub(crate) eltoo: Arc<EltooHandler>,
}

impl BaseMessageHandler for DualChannelMessageHandler {
	fn get_and_clear_pending_msg_events(&self) -> Vec<MessageSendEvent> {
		let mut events = self.ldk.get_and_clear_pending_msg_events();
		events.append(&mut self.eltoo.get_and_clear_pending_msg_events());
		events
	}

	fn peer_connected(
		&self, their_node_id: PublicKey, msg: &msgs::Init, inbound: bool,
	) -> Result<(), ()> {
		self.eltoo.peer_connected(their_node_id, msg, inbound)?;
		self.ldk.peer_connected(their_node_id, msg, inbound)
	}

	fn peer_disconnected(&self, their_node_id: PublicKey) {
		self.eltoo.peer_disconnected(their_node_id);
		self.ldk.peer_disconnected(their_node_id);
	}

	fn provided_node_features(&self) -> NodeFeatures {
		let mut features = self.ldk.provided_node_features();
		features.set_eltoo_optional();
		features
	}

	fn provided_init_features(&self, their_node_id: PublicKey) -> InitFeatures {
		let mut features = self.ldk.provided_init_features(their_node_id);
		features.set_eltoo_optional();
		features
	}
}

macro_rules! delegate_to_ldk {
	($($method:ident($msg:ty)),* $(,)?) => {
		$(
			fn $method(&self, their_node_id: PublicKey, msg: $msg) {
				self.ldk.$method(their_node_id, msg);
			}
		)*
	};
}

macro_rules! delegate_to_eltoo {
	($($method:ident($msg:ty)),* $(,)?) => {
		$(
			fn $method(&self, their_node_id: PublicKey, msg: $msg) {
				self.eltoo.$method(their_node_id, msg);
			}
		)*
	};
}

impl ChannelMessageHandler for DualChannelMessageHandler {
	delegate_to_ldk!(
		handle_open_channel(&msgs::OpenChannel),
		handle_open_channel_v2(&msgs::OpenChannelV2),
		handle_accept_channel(&msgs::AcceptChannel),
		handle_accept_channel_v2(&msgs::AcceptChannelV2),
		handle_funding_created(&msgs::FundingCreated),
		handle_funding_signed(&msgs::FundingSigned),
		handle_channel_ready(&msgs::ChannelReady),
		handle_peer_storage(msgs::PeerStorage),
		handle_peer_storage_retrieval(msgs::PeerStorageRetrieval),
		handle_shutdown(&msgs::Shutdown),
		handle_closing_signed(&msgs::ClosingSigned),
		handle_stfu(&msgs::Stfu),
		handle_splice_init(&msgs::SpliceInit),
		handle_splice_ack(&msgs::SpliceAck),
		handle_splice_locked(&msgs::SpliceLocked),
		handle_tx_add_input(&msgs::TxAddInput),
		handle_tx_add_output(&msgs::TxAddOutput),
		handle_tx_remove_input(&msgs::TxRemoveInput),
		handle_tx_remove_output(&msgs::TxRemoveOutput),
		handle_tx_complete(&msgs::TxComplete),
		handle_tx_signatures(&msgs::TxSignatures),
		handle_tx_init_rbf(&msgs::TxInitRbf),
		handle_tx_ack_rbf(&msgs::TxAckRbf),
		handle_tx_abort(&msgs::TxAbort),
		handle_update_fail_malformed_htlc(&msgs::UpdateFailMalformedHTLC),
		handle_commitment_signed(&msgs::CommitmentSigned),
		handle_revoke_and_ack(&msgs::RevokeAndACK),
		handle_update_fee(&msgs::UpdateFee),
		handle_announcement_signatures(&msgs::AnnouncementSignatures),
		handle_channel_reestablish(&msgs::ChannelReestablish),
		handle_channel_update(&msgs::ChannelUpdate),
		handle_error(&msgs::ErrorMessage),
	);

	fn handle_commitment_signed_batch(
		&self, their_node_id: PublicKey, channel_id: ChannelId, batch: Vec<msgs::CommitmentSigned>,
	) {
		self.ldk.handle_commitment_signed_batch(their_node_id, channel_id, batch);
	}

	delegate_to_eltoo!(
		handle_open_channel_eltoo(&lightning::ln::eltoo::msgs::OpenChannelEltoo),
		handle_accept_channel_eltoo(&lightning::ln::eltoo::msgs::AcceptChannelEltoo),
		handle_funding_created_eltoo(&lightning::ln::eltoo::msgs::FundingCreatedEltoo),
		handle_funding_signed_eltoo(&lightning::ln::eltoo::msgs::FundingSignedEltoo),
		handle_channel_ready_eltoo(&lightning::ln::eltoo::msgs::ChannelReadyEltoo),
		handle_shutdown_eltoo(&lightning::ln::eltoo::msgs::ShutdownEltoo),
		handle_closing_signed_eltoo(&lightning::ln::eltoo::msgs::ClosingSignedEltoo),
		handle_update_signed(&lightning::ln::eltoo::msgs::UpdateSigned),
		handle_update_signed_ack(&lightning::ln::eltoo::msgs::UpdateSignedAck),
		handle_channel_reestablish_eltoo(&lightning::ln::eltoo::msgs::ChannelReestablishEltoo),
		handle_update_noop(&lightning::ln::eltoo::msgs::UpdateNoop),
		handle_yield(&lightning::ln::eltoo::msgs::Yield),
	);

	// The BOLT 2 HTLC update messages are shared between the two protocols. Route by
	// which manager knows the channel; the regular manager takes precedence.
	fn handle_update_add_htlc(&self, their_node_id: PublicKey, msg: &msgs::UpdateAddHTLC) {
		if self.is_ldk_channel(msg.channel_id) {
			self.ldk.handle_update_add_htlc(their_node_id, msg);
		} else {
			self.eltoo.handle_update_add_htlc(their_node_id, msg);
		}
	}
	fn handle_update_fulfill_htlc(&self, their_node_id: PublicKey, msg: msgs::UpdateFulfillHTLC) {
		if self.is_ldk_channel(msg.channel_id) {
			self.ldk.handle_update_fulfill_htlc(their_node_id, msg);
		} else {
			self.eltoo.handle_update_fulfill_htlc(their_node_id, msg);
		}
	}
	fn handle_update_fail_htlc(&self, their_node_id: PublicKey, msg: &msgs::UpdateFailHTLC) {
		if self.is_ldk_channel(msg.channel_id) {
			self.ldk.handle_update_fail_htlc(their_node_id, msg);
		} else {
			self.eltoo.handle_update_fail_htlc(their_node_id, msg);
		}
	}

	fn get_chain_hashes(&self) -> Option<Vec<ChainHash>> {
		self.ldk.get_chain_hashes()
	}

	fn message_received(&self) {
		self.ldk.message_received();
	}
}

impl DualChannelMessageHandler {
	fn is_ldk_channel(&self, channel_id: ChannelId) -> bool {
		self.ldk.list_channels().iter().any(|details| details.channel_id == channel_id)
	}
}
