// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::secp256k1::PublicKey;
use bitcoin::{BlockHash, Txid};
use lightning::chain::chaininterface::TransactionType as LdkTransactionType;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::DecodeError;
use lightning::ln::types::ChannelId;
use lightning::offers::offer::OfferId;
use lightning::util::ser::{Readable, Writeable};
use lightning::{
	_init_and_read_len_prefixed_tlv_fields, impl_writeable_tlv_based,
	impl_writeable_tlv_based_enum, write_tlv_fields,
};
use lightning_types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning_types::string::UntrustedString;

use crate::data_store::{StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;

/// A helper struct pairing a counterparty node id with a channel id.
///
/// This is used in [`TransactionType`] variants that may reference one or more channels.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Channel {
	/// The `node_id` of the channel counterparty.
	pub counterparty_node_id: PublicKey,
	/// The ID of the channel.
	pub channel_id: ChannelId,
}

impl_writeable_tlv_based!(Channel, {
	(0, counterparty_node_id, required),
	(2, channel_id, required),
});

/// Represents the class of transaction being broadcast.
///
/// This is used to provide context about the type of transaction being broadcast, which may be
/// useful for logging, filtering, or prioritization purposes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionType {
	/// A funding transaction establishing a new channel.
	Funding {
		/// The counterparty node IDs and channel IDs of the channels being funded.
		///
		/// A single funding transaction may establish multiple channels when using batch funding.
		channels: Vec<Channel>,
	},
	/// A transaction cooperatively closing a channel.
	CooperativeClose {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel being closed.
		channel_id: ChannelId,
	},
	/// A transaction being broadcast to force-close the channel.
	UnilateralClose {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel being force-closed.
		channel_id: ChannelId,
	},
	/// An anchor bumping transaction used for CPFP fee-bumping a closing transaction.
	AnchorBump {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel whose closing transaction is being fee-bumped.
		channel_id: ChannelId,
	},
	/// A transaction which is resolving an output spendable by both us and our counterparty.
	///
	/// When a channel closes via the unilateral close path, there may be transaction outputs which
	/// are spendable by either our counterparty or us and represent some lightning state. In order
	/// to resolve that state, any such outputs will be spent, ensuring funds are only available to
	/// us. This transaction is one such transaction - resolving in-flight HTLCs or punishing our
	/// counterparty if they broadcasted an outdated state.
	Claim {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel from which outputs are being claimed.
		channel_id: ChannelId,
	},
	/// A transaction sweeping spendable outputs to the user's wallet.
	Sweep {
		/// The counterparty node IDs and channel IDs from which outputs are being swept, if known.
		///
		/// A single sweep transaction may aggregate outputs from multiple channels.
		channels: Vec<Channel>,
	},
	/// A splice transaction modifying an existing channel's funding.
	Splice {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel being spliced.
		channel_id: ChannelId,
	},
	/// A user-initiated on-chain payment to an external address.
	OnchainSend,
}

impl_writeable_tlv_based_enum!(TransactionType,
	(0, Funding) => { (0, channels, optional_vec) },
	(2, CooperativeClose) => { (0, counterparty_node_id, required), (2, channel_id, required) },
	(4, UnilateralClose) => { (0, counterparty_node_id, required), (2, channel_id, required) },
	(6, AnchorBump) => { (0, counterparty_node_id, required), (2, channel_id, required) },
	(8, Claim) => { (0, counterparty_node_id, required), (2, channel_id, required) },
	(10, Sweep) => { (0, channels, optional_vec) },
	(12, Splice) => { (0, counterparty_node_id, required), (2, channel_id, required) },
	(14, OnchainSend) => {}
);

impl From<LdkTransactionType> for TransactionType {
	fn from(ldk_type: LdkTransactionType) -> Self {
		match ldk_type {
			LdkTransactionType::Funding { channels } => TransactionType::Funding {
				channels: channels
					.into_iter()
					.map(|(node_id, chan_id)| Channel {
						counterparty_node_id: node_id,
						channel_id: chan_id,
					})
					.collect(),
			},
			LdkTransactionType::CooperativeClose { counterparty_node_id, channel_id } => {
				TransactionType::CooperativeClose { counterparty_node_id, channel_id }
			},
			LdkTransactionType::UnilateralClose { counterparty_node_id, channel_id } => {
				TransactionType::UnilateralClose { counterparty_node_id, channel_id }
			},
			LdkTransactionType::AnchorBump { counterparty_node_id, channel_id } => {
				TransactionType::AnchorBump { counterparty_node_id, channel_id }
			},
			LdkTransactionType::Claim { counterparty_node_id, channel_id } => {
				TransactionType::Claim { counterparty_node_id, channel_id }
			},
			LdkTransactionType::Sweep { channels } => TransactionType::Sweep {
				channels: channels
					.into_iter()
					.map(|(node_id, chan_id)| Channel {
						counterparty_node_id: node_id,
						channel_id: chan_id,
					})
					.collect(),
			},
			LdkTransactionType::Splice { counterparty_node_id, channel_id } => {
				TransactionType::Splice { counterparty_node_id, channel_id }
			},
		}
	}
}

/// Represents a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentDetails {
	/// The identifier of this payment.
	pub id: PaymentId,
	/// The kind of the payment.
	pub kind: PaymentKind,
	/// The amount transferred.
	///
	/// Will be `None` for variable-amount payments until we receive them.
	pub amount_msat: Option<u64>,
	/// The fees that were paid for this payment.
	///
	/// For Lightning payments, this will only be updated for outbound payments once they
	/// succeeded.
	///
	/// Will be `None` for Lightning payments made with LDK Node v0.4.x and earlier.
	pub fee_paid_msat: Option<u64>,
	/// The direction of the payment.
	pub direction: PaymentDirection,
	/// The status of the payment.
	pub status: PaymentStatus,
	/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
	pub latest_update_timestamp: u64,
}

impl PaymentDetails {
	pub(crate) fn new(
		id: PaymentId, kind: PaymentKind, amount_msat: Option<u64>, fee_paid_msat: Option<u64>,
		direction: PaymentDirection, status: PaymentStatus,
	) -> Self {
		let latest_update_timestamp = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();
		Self { id, kind, amount_msat, fee_paid_msat, direction, status, latest_update_timestamp }
	}
}

impl Writeable for PaymentDetails {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		write_tlv_fields!(writer, {
			(0, self.id, required), // Used to be `hash` for v0.2.1 and prior
			// 1 briefly used to be lsp_fee_limits, could probably be reused at some point in the future.
			// 2 used to be `preimage` before it was moved to `kind` in v0.3.0
			(2, None::<Option<PaymentPreimage>>, required),
			(3, self.kind, required),
			// 4 used to be `secret` before it was moved to `kind` in v0.3.0
			(4, None::<Option<PaymentSecret>>, required),
			(5, self.latest_update_timestamp, required),
			(6, self.amount_msat, required),
			(7, self.fee_paid_msat, option),
			(8, self.direction, required),
			(10, self.status, required)
		});
		Ok(())
	}
}

impl Readable for PaymentDetails {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<PaymentDetails, DecodeError> {
		let unix_time_secs = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or(Duration::from_secs(0))
			.as_secs();
		_init_and_read_len_prefixed_tlv_fields!(reader, {
			(0, id, required), // Used to be `hash`
			(1, lsp_fee_limits, option),
			(2, preimage, required),
			(3, kind_opt, option),
			(4, secret, required),
			(5, latest_update_timestamp, (default_value, unix_time_secs)),
			(6, amount_msat, required),
			(7, fee_paid_msat, option),
			(8, direction, required),
			(10, status, required)
		});

		let id: PaymentId = id.0.ok_or(DecodeError::InvalidValue)?;
		let preimage: Option<PaymentPreimage> = preimage.0.ok_or(DecodeError::InvalidValue)?;
		let secret: Option<PaymentSecret> = secret.0.ok_or(DecodeError::InvalidValue)?;
		let latest_update_timestamp: u64 =
			latest_update_timestamp.0.ok_or(DecodeError::InvalidValue)?;
		let amount_msat: Option<u64> = amount_msat.0.ok_or(DecodeError::InvalidValue)?;
		let direction: PaymentDirection = direction.0.ok_or(DecodeError::InvalidValue)?;
		let status: PaymentStatus = status.0.ok_or(DecodeError::InvalidValue)?;

		let kind = if let Some(kind) = kind_opt {
			// If we serialized the payment kind, use it.
			// This will always be the case for any version after v0.2.1.
			kind
		} else {
			// Otherwise we persisted with v0.2.1 or before, and puzzle together the kind from the
			// provided fields.

			// We used to track everything by hash, but switched to track everything by id
			// post-v0.2.1. As both are serialized identically, we just switched the `0`-type field above
			// from `PaymentHash` to `PaymentId` and serialize a separate `PaymentHash` in
			// `PaymentKind` when needed. Here, for backwards compat, we can just re-create the
			// `PaymentHash` from the id, as 'back then' `payment_hash == payment_id` was always
			// true.
			let hash = PaymentHash(id.0);

			if secret.is_some() {
				if let Some(lsp_fee_limits) = lsp_fee_limits {
					let counterparty_skimmed_fee_msat = None;
					PaymentKind::Bolt11Jit {
						hash,
						preimage,
						secret,
						counterparty_skimmed_fee_msat,
						lsp_fee_limits,
					}
				} else {
					PaymentKind::Bolt11 { hash, preimage, secret }
				}
			} else {
				PaymentKind::Spontaneous { hash, preimage }
			}
		};

		Ok(PaymentDetails {
			id,
			kind,
			amount_msat,
			fee_paid_msat,
			direction,
			status,
			latest_update_timestamp,
		})
	}
}

impl StorableObjectId for PaymentId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}
impl StorableObject for PaymentDetails {
	type Id = PaymentId;
	type Update = PaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, update: &Self::Update) -> bool {
		debug_assert_eq!(
			self.id, update.id,
			"We should only ever override payment data for the same payment id"
		);

		let mut updated = false;

		macro_rules! update_if_necessary {
			($val:expr, $update:expr) => {
				if $val != $update {
					$val = $update;
					updated = true;
				}
			};
		}

		if let Some(hash_opt) = update.hash {
			match self.kind {
				PaymentKind::Bolt12Offer { ref mut hash, .. } => {
					debug_assert_eq!(
						self.direction,
						PaymentDirection::Outbound,
						"We should only ever override payment hash for outbound BOLT 12 payments"
					);
					debug_assert!(
						hash.is_none() || *hash == hash_opt,
						"We should never change a payment hash after being initially set"
					);
					update_if_necessary!(*hash, hash_opt);
				},
				PaymentKind::Bolt12Refund { ref mut hash, .. } => {
					debug_assert_eq!(
						self.direction,
						PaymentDirection::Outbound,
						"We should only ever override payment hash for outbound BOLT 12 payments"
					);
					debug_assert!(
						hash.is_none() || *hash == hash_opt,
						"We should never change a payment hash after being initially set"
					);
					update_if_necessary!(*hash, hash_opt);
				},
				_ => {
					// We can omit updating the hash for BOLT11 payments as the payment hash
					// will always be known from the beginning.
				},
			}
		}
		if let Some(preimage_opt) = update.preimage {
			match self.kind {
				PaymentKind::Bolt11 { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				PaymentKind::Bolt11Jit { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				PaymentKind::Bolt12Offer { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				PaymentKind::Bolt12Refund { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				PaymentKind::Spontaneous { ref mut preimage, .. } => {
					update_if_necessary!(*preimage, preimage_opt)
				},
				_ => {},
			}
		}

		if let Some(secret_opt) = update.secret {
			match self.kind {
				PaymentKind::Bolt11 { ref mut secret, .. } => {
					update_if_necessary!(*secret, secret_opt)
				},
				PaymentKind::Bolt11Jit { ref mut secret, .. } => {
					update_if_necessary!(*secret, secret_opt)
				},
				PaymentKind::Bolt12Offer { ref mut secret, .. } => {
					update_if_necessary!(*secret, secret_opt)
				},
				PaymentKind::Bolt12Refund { ref mut secret, .. } => {
					update_if_necessary!(*secret, secret_opt)
				},
				_ => {},
			}
		}

		if let Some(amount_opt) = update.amount_msat {
			update_if_necessary!(self.amount_msat, amount_opt);
		}

		if let Some(fee_paid_msat_opt) = update.fee_paid_msat {
			update_if_necessary!(self.fee_paid_msat, fee_paid_msat_opt);
		}

		if let Some(skimmed_fee_msat) = update.counterparty_skimmed_fee_msat {
			match self.kind {
				PaymentKind::Bolt11Jit { ref mut counterparty_skimmed_fee_msat, .. } => {
					update_if_necessary!(*counterparty_skimmed_fee_msat, skimmed_fee_msat);
				},
				_ => debug_assert!(
					false,
					"We should only ever override counterparty_skimmed_fee_msat for JIT payments"
				),
			}
		}

		if let Some(status) = update.status {
			update_if_necessary!(self.status, status);
		}

		if let Some(confirmation_status) = update.confirmation_status {
			match self.kind {
				PaymentKind::Onchain { ref mut status, .. } => {
					update_if_necessary!(*status, confirmation_status);
				},
				_ => {},
			}
		}

		if let Some(Some(ref new_tx_type)) = update.tx_type {
			match self.kind {
				PaymentKind::Onchain { ref mut tx_type, .. } => {
					debug_assert!(
						tx_type.is_none() || tx_type.as_ref() == Some(new_tx_type),
						"We should never change a transaction type after being initially set"
					);
					update_if_necessary!(*tx_type, Some(new_tx_type.clone()));
				},
				_ => {},
			}
		}

		if updated {
			self.latest_update_timestamp = SystemTime::now()
				.duration_since(UNIX_EPOCH)
				.unwrap_or(Duration::from_secs(0))
				.as_secs();
		}

		updated
	}

	fn to_update(&self) -> Self::Update {
		self.into()
	}
}

/// Represents the direction of a payment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PaymentDirection {
	/// The payment is inbound.
	Inbound,
	/// The payment is outbound.
	Outbound,
}

impl_writeable_tlv_based_enum!(PaymentDirection,
	(0, Inbound) => {},
	(1, Outbound) => {}
);

/// Represents the current status of a payment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PaymentStatus {
	/// The payment is still pending.
	Pending,
	/// The payment succeeded.
	Succeeded,
	/// The payment failed.
	Failed,
}

impl_writeable_tlv_based_enum!(PaymentStatus,
	(0, Pending) => {},
	(2, Succeeded) => {},
	(4, Failed) => {}
);

/// Represents the kind of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaymentKind {
	/// An on-chain payment.
	///
	/// Payments of this kind will be considered pending until the respective transaction has
	/// reached [`ANTI_REORG_DELAY`] confirmations on-chain.
	///
	/// [`ANTI_REORG_DELAY`]: lightning::chain::channelmonitor::ANTI_REORG_DELAY
	Onchain {
		/// The transaction identifier of this payment.
		txid: Txid,
		/// The confirmation status of this payment.
		status: ConfirmationStatus,
		/// The type of the on-chain transaction, if known.
		tx_type: Option<TransactionType>,
	},
	/// A [BOLT 11] payment.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	Bolt11 {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
	},
	/// A [BOLT 11] payment intended to open an [bLIP-52 / LSPS 2] just-in-time channel.
	///
	/// [BOLT 11]: https://github.com/lightning/bolts/blob/master/11-payment-encoding.md
	/// [bLIP-52 / LSPS2]: https://github.com/lightning/blips/blob/master/blip-0052.md
	Bolt11Jit {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
		/// The value, in thousands of a satoshi, that was deducted from this payment as an extra
		/// fee taken by our channel counterparty.
		///
		/// Will only be `Some` once we received the payment. Will always be `None` for LDK Node
		/// v0.4 and prior.
		counterparty_skimmed_fee_msat: Option<u64>,
		/// Limits applying to how much fee we allow an LSP to deduct from the payment amount.
		///
		/// Allowing them to deduct this fee from the first inbound payment will pay for the LSP's
		/// channel opening fees.
		///
		/// See [`LdkChannelConfig::accept_underpaying_htlcs`] for more information.
		///
		/// [`LdkChannelConfig::accept_underpaying_htlcs`]: lightning::util::config::ChannelConfig::accept_underpaying_htlcs
		lsp_fee_limits: LSPFeeLimits,
	},
	/// A [BOLT 12] 'offer' payment, i.e., a payment for an [`Offer`].
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [`Offer`]: crate::lightning::offers::offer::Offer
	Bolt12Offer {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: Option<PaymentHash>,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
		/// The ID of the offer this payment is for.
		offer_id: OfferId,
		/// The payer note for the payment.
		///
		/// Truncated to [`PAYER_NOTE_LIMIT`] characters.
		///
		/// This will always be `None` for payments serialized with version `v0.3.0`.
		///
		/// [`PAYER_NOTE_LIMIT`]: lightning::offers::invoice_request::PAYER_NOTE_LIMIT
		payer_note: Option<UntrustedString>,
		/// The quantity of an item requested in the offer.
		///
		/// This will always be `None` for payments serialized with version `v0.3.0`.
		quantity: Option<u64>,
	},
	/// A [BOLT 12] 'refund' payment, i.e., a payment for a [`Refund`].
	///
	/// [BOLT 12]: https://github.com/lightning/bolts/blob/master/12-offer-encoding.md
	/// [`Refund`]: lightning::offers::refund::Refund
	Bolt12Refund {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: Option<PaymentHash>,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
		/// The secret used by the payment.
		secret: Option<PaymentSecret>,
		/// The payer note for the refund payment.
		///
		/// This will always be `None` for payments serialized with version `v0.3.0`.
		payer_note: Option<UntrustedString>,
		/// The quantity of an item that the refund is for.
		///
		/// This will always be `None` for payments serialized with version `v0.3.0`.
		quantity: Option<u64>,
	},
	/// A spontaneous ("keysend") payment.
	Spontaneous {
		/// The payment hash, i.e., the hash of the `preimage`.
		hash: PaymentHash,
		/// The pre-image used by the payment.
		preimage: Option<PaymentPreimage>,
	},
}

impl_writeable_tlv_based_enum!(PaymentKind,
	(0, Onchain) => {
		(0, txid, required),
		(1, tx_type, option),
		(2, status, required),
	},
	(2, Bolt11) => {
		(0, hash, required),
		(2, preimage, option),
		(4, secret, option),
	},
	(4, Bolt11Jit) => {
		(0, hash, required),
		(1, counterparty_skimmed_fee_msat, option),
		(2, preimage, option),
		(4, secret, option),
		(6, lsp_fee_limits, required),
	},
	(6, Bolt12Offer) => {
		(0, hash, option),
		(1, payer_note, option),
		(2, preimage, option),
		(3, quantity, option),
		(4, secret, option),
		(6, offer_id, required),
	},
	(8, Spontaneous) => {
		(0, hash, required),
		(2, preimage, option),
	},
	(10, Bolt12Refund) => {
		(0, hash, option),
		(1, payer_note, option),
		(2, preimage, option),
		(3, quantity, option),
		(4, secret, option),
	}
);

/// Represents the confirmation status of a transaction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConfirmationStatus {
	/// The transaction is confirmed in the best chain.
	Confirmed {
		/// The hash of the block in which the transaction was confirmed.
		block_hash: BlockHash,
		/// The height under which the block was confirmed.
		height: u32,
		/// The timestamp, in seconds since start of the UNIX epoch, when this entry was last updated.
		timestamp: u64,
	},
	/// The transaction is unconfirmed.
	Unconfirmed,
}

impl_writeable_tlv_based_enum!(ConfirmationStatus,
	(0, Confirmed) => {
		(0, block_hash, required),
		(2, height, required),
		(4, timestamp, required),
	},
	(2, Unconfirmed) => {},
);

/// Limits applying to how much fee we allow an LSP to deduct from the payment amount.
///
/// See [`LdkChannelConfig::accept_underpaying_htlcs`] for more information.
///
/// [`LdkChannelConfig::accept_underpaying_htlcs`]: lightning::util::config::ChannelConfig::accept_underpaying_htlcs
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LSPFeeLimits {
	/// The maximal total amount we allow any configured LSP withhold from us when forwarding the
	/// payment.
	pub max_total_opening_fee_msat: Option<u64>,
	/// The maximal proportional fee, in parts-per-million millisatoshi, we allow any configured
	/// LSP withhold from us when forwarding the payment.
	pub max_proportional_opening_fee_ppm_msat: Option<u64>,
}

impl_writeable_tlv_based!(LSPFeeLimits, {
	(0, max_total_opening_fee_msat, option),
	(2, max_proportional_opening_fee_ppm_msat, option),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentDetailsUpdate {
	pub id: PaymentId,
	pub hash: Option<Option<PaymentHash>>,
	pub preimage: Option<Option<PaymentPreimage>>,
	pub secret: Option<Option<PaymentSecret>>,
	pub amount_msat: Option<Option<u64>>,
	pub fee_paid_msat: Option<Option<u64>>,
	pub counterparty_skimmed_fee_msat: Option<Option<u64>>,
	pub direction: Option<PaymentDirection>,
	pub status: Option<PaymentStatus>,
	pub confirmation_status: Option<ConfirmationStatus>,
	pub tx_type: Option<Option<TransactionType>>,
}

impl PaymentDetailsUpdate {
	pub fn new(id: PaymentId) -> Self {
		Self {
			id,
			hash: None,
			preimage: None,
			secret: None,
			amount_msat: None,
			fee_paid_msat: None,
			counterparty_skimmed_fee_msat: None,
			direction: None,
			status: None,
			confirmation_status: None,
			tx_type: None,
		}
	}
}

impl From<&PaymentDetails> for PaymentDetailsUpdate {
	fn from(value: &PaymentDetails) -> Self {
		let (hash, preimage, secret) = match value.kind {
			PaymentKind::Bolt11 { hash, preimage, secret, .. } => (Some(hash), preimage, secret),
			PaymentKind::Bolt11Jit { hash, preimage, secret, .. } => (Some(hash), preimage, secret),
			PaymentKind::Bolt12Offer { hash, preimage, secret, .. } => (hash, preimage, secret),
			PaymentKind::Bolt12Refund { hash, preimage, secret, .. } => (hash, preimage, secret),
			PaymentKind::Spontaneous { hash, preimage, .. } => (Some(hash), preimage, None),
			_ => (None, None, None),
		};

		let (confirmation_status, tx_type) = match value.kind {
			PaymentKind::Onchain { status, ref tx_type, .. } => {
				(Some(status), Some(tx_type.clone()))
			},
			_ => (None, None),
		};

		let counterparty_skimmed_fee_msat = match value.kind {
			PaymentKind::Bolt11Jit { counterparty_skimmed_fee_msat, .. } => {
				Some(counterparty_skimmed_fee_msat)
			},
			_ => None,
		};

		Self {
			id: value.id,
			hash: Some(hash),
			preimage: Some(preimage),
			secret: Some(secret),
			amount_msat: Some(value.amount_msat),
			fee_paid_msat: Some(value.fee_paid_msat),
			counterparty_skimmed_fee_msat,
			direction: Some(value.direction),
			status: Some(value.status),
			confirmation_status,
			tx_type,
		}
	}
}

impl StorableObjectUpdate<PaymentDetails> for PaymentDetailsUpdate {
	fn id(&self) -> <PaymentDetails as StorableObject>::Id {
		self.id
	}
}

#[cfg(test)]
mod tests {
	use lightning::util::ser::Readable;

	use super::*;

	/// We refactored `PaymentDetails` to hold a payment id and moved some required fields into
	/// `PaymentKind`. Here, we keep the old layout available in order test de/ser compatibility.
	#[derive(Clone, Debug, PartialEq, Eq)]
	struct OldPaymentDetails {
		pub hash: PaymentHash,
		pub preimage: Option<PaymentPreimage>,
		pub secret: Option<PaymentSecret>,
		pub amount_msat: Option<u64>,
		pub direction: PaymentDirection,
		pub status: PaymentStatus,
		pub lsp_fee_limits: Option<LSPFeeLimits>,
	}

	impl_writeable_tlv_based!(OldPaymentDetails, {
		(0, hash, required),
		(1, lsp_fee_limits, option),
		(2, preimage, required),
		(4, secret, required),
		(6, amount_msat, required),
		(8, direction, required),
		(10, status, required)
	});

	#[test]
	fn old_payment_details_deser_compat() {
		// We refactored `PaymentDetails` to hold a payment id and moved some required fields into
		// `PaymentKind`. Here, we test compatibility with the old layout.
		let hash = PaymentHash([42u8; 32]);
		let preimage = Some(PaymentPreimage([43u8; 32]));
		let secret = Some(PaymentSecret([44u8; 32]));
		let amount_msat = Some(45_000_000);

		// Test `Bolt11` de/ser
		{
			let old_bolt11_payment = OldPaymentDetails {
				hash,
				preimage,
				secret,
				amount_msat,
				direction: PaymentDirection::Inbound,
				status: PaymentStatus::Pending,
				lsp_fee_limits: None,
			};

			let old_bolt11_encoded = old_bolt11_payment.encode();
			assert_eq!(
				old_bolt11_payment,
				OldPaymentDetails::read(&mut &*old_bolt11_encoded.clone()).unwrap()
			);

			let bolt11_decoded = PaymentDetails::read(&mut &*old_bolt11_encoded).unwrap();
			let bolt11_reencoded = bolt11_decoded.encode();
			assert_eq!(bolt11_decoded, PaymentDetails::read(&mut &*bolt11_reencoded).unwrap());

			match bolt11_decoded.kind {
				PaymentKind::Bolt11 { hash: h, preimage: p, secret: s } => {
					assert_eq!(hash, h);
					assert_eq!(preimage, p);
					assert_eq!(secret, s);
				},
				_ => {
					panic!("Unexpected kind!");
				},
			}
		}

		// Test `Bolt11Jit` de/ser
		{
			let lsp_fee_limits = Some(LSPFeeLimits {
				max_total_opening_fee_msat: Some(46_000),
				max_proportional_opening_fee_ppm_msat: Some(47_000),
			});

			let old_bolt11_jit_payment = OldPaymentDetails {
				hash,
				preimage,
				secret,
				amount_msat,
				direction: PaymentDirection::Inbound,
				status: PaymentStatus::Pending,
				lsp_fee_limits,
			};

			let old_bolt11_jit_encoded = old_bolt11_jit_payment.encode();
			assert_eq!(
				old_bolt11_jit_payment,
				OldPaymentDetails::read(&mut &*old_bolt11_jit_encoded.clone()).unwrap()
			);

			let bolt11_jit_decoded = PaymentDetails::read(&mut &*old_bolt11_jit_encoded).unwrap();
			let bolt11_jit_reencoded = bolt11_jit_decoded.encode();
			assert_eq!(
				bolt11_jit_decoded,
				PaymentDetails::read(&mut &*bolt11_jit_reencoded).unwrap()
			);

			match bolt11_jit_decoded.kind {
				PaymentKind::Bolt11Jit {
					hash: h,
					preimage: p,
					secret: s,
					counterparty_skimmed_fee_msat: c,
					lsp_fee_limits: l,
				} => {
					assert_eq!(hash, h);
					assert_eq!(preimage, p);
					assert_eq!(secret, s);
					assert_eq!(None, c);
					assert_eq!(lsp_fee_limits, Some(l));
				},
				_ => {
					panic!("Unexpected kind!");
				},
			}
		}

		// Test `Spontaneous` de/ser
		{
			let old_spontaneous_payment = OldPaymentDetails {
				hash,
				preimage,
				secret: None,
				amount_msat,
				direction: PaymentDirection::Inbound,
				status: PaymentStatus::Pending,
				lsp_fee_limits: None,
			};

			let old_spontaneous_encoded = old_spontaneous_payment.encode();
			assert_eq!(
				old_spontaneous_payment,
				OldPaymentDetails::read(&mut &*old_spontaneous_encoded.clone()).unwrap()
			);

			let spontaneous_decoded = PaymentDetails::read(&mut &*old_spontaneous_encoded).unwrap();
			let spontaneous_reencoded = spontaneous_decoded.encode();
			assert_eq!(
				spontaneous_decoded,
				PaymentDetails::read(&mut &*spontaneous_reencoded).unwrap()
			);

			match spontaneous_decoded.kind {
				PaymentKind::Spontaneous { hash: h, preimage: p } => {
					assert_eq!(hash, h);
					assert_eq!(preimage, p);
				},
				_ => {
					panic!("Unexpected kind!");
				},
			}
		}
	}

	#[test]
	fn transaction_type_serialization_roundtrip() {
		use lightning::util::ser::Writeable;

		let pubkey = PublicKey::from_slice(&[
			2, 238, 199, 36, 93, 107, 125, 44, 203, 48, 56, 11, 251, 226, 163, 100, 140, 215, 169,
			66, 101, 63, 90, 163, 64, 237, 206, 161, 242, 131, 104, 102, 25,
		])
		.unwrap();
		let channel_id = ChannelId([42u8; 32]);
		let channel = Channel { counterparty_node_id: pubkey, channel_id };

		// Test all variants
		let variants: Vec<TransactionType> = vec![
			TransactionType::Funding { channels: vec![channel.clone()] },
			TransactionType::CooperativeClose { counterparty_node_id: pubkey, channel_id },
			TransactionType::UnilateralClose { counterparty_node_id: pubkey, channel_id },
			TransactionType::AnchorBump { counterparty_node_id: pubkey, channel_id },
			TransactionType::Claim { counterparty_node_id: pubkey, channel_id },
			TransactionType::Sweep { channels: vec![channel.clone()] },
			TransactionType::Splice { counterparty_node_id: pubkey, channel_id },
			TransactionType::OnchainSend,
			// Also test empty channel lists
			TransactionType::Funding { channels: vec![] },
			TransactionType::Sweep { channels: vec![] },
		];

		for variant in variants {
			let encoded = variant.encode();
			let decoded = TransactionType::read(&mut &*encoded).unwrap();
			assert_eq!(variant, decoded);
		}
	}

	#[test]
	fn onchain_payment_kind_backward_compat() {
		// Simulate the old serialization format for PaymentKind::Onchain (without tx_type).
		// The old format only had fields (0, txid) and (2, status).
		use bitcoin::hashes::Hash;
		use lightning::util::ser::Writeable;

		let txid = bitcoin::Txid::from_slice(&[42u8; 32]).unwrap();
		let status = ConfirmationStatus::Unconfirmed;

		// Manually create a PaymentKind::Onchain with tx_type = None and verify roundtrip.
		let kind = PaymentKind::Onchain { txid, status, tx_type: None };
		let encoded = kind.encode();
		let decoded = PaymentKind::read(&mut &*encoded).unwrap();
		assert_eq!(kind, decoded);

		// Also test with tx_type = Some
		let kind_with_type =
			PaymentKind::Onchain { txid, status, tx_type: Some(TransactionType::OnchainSend) };
		let encoded_with_type = kind_with_type.encode();
		let decoded_with_type = PaymentKind::read(&mut &*encoded_with_type).unwrap();
		assert_eq!(kind_with_type, decoded_with_type);

		// Verify that the encoding without tx_type is shorter (backward compat: old readers
		// skip the unknown odd field).
		assert!(encoded.len() < encoded_with_type.len());
	}

	#[test]
	fn from_ldk_transaction_type() {
		let pubkey = PublicKey::from_slice(&[
			2, 238, 199, 36, 93, 107, 125, 44, 203, 48, 56, 11, 251, 226, 163, 100, 140, 215, 169,
			66, 101, 63, 90, 163, 64, 237, 206, 161, 242, 131, 104, 102, 25,
		])
		.unwrap();
		let channel_id = ChannelId([42u8; 32]);

		// Test Funding conversion
		let ldk_funding = LdkTransactionType::Funding { channels: vec![(pubkey, channel_id)] };
		let local_funding = TransactionType::from(ldk_funding);
		assert_eq!(
			local_funding,
			TransactionType::Funding {
				channels: vec![Channel { counterparty_node_id: pubkey, channel_id }]
			}
		);

		// Test CooperativeClose conversion
		let ldk_coop =
			LdkTransactionType::CooperativeClose { counterparty_node_id: pubkey, channel_id };
		let local_coop = TransactionType::from(ldk_coop);
		assert_eq!(
			local_coop,
			TransactionType::CooperativeClose { counterparty_node_id: pubkey, channel_id }
		);

		// Test Sweep conversion with empty channels
		let ldk_sweep = LdkTransactionType::Sweep { channels: vec![] };
		let local_sweep = TransactionType::from(ldk_sweep);
		assert_eq!(local_sweep, TransactionType::Sweep { channels: vec![] });

		// Test Splice conversion
		let ldk_splice = LdkTransactionType::Splice { counterparty_node_id: pubkey, channel_id };
		let local_splice = TransactionType::from(ldk_splice);
		assert_eq!(
			local_splice,
			TransactionType::Splice { counterparty_node_id: pubkey, channel_id }
		);
	}
}
