// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::secp256k1::PublicKey;
use bitcoin::{BlockHash, Txid};
use lightning::chain::chaininterface::TransactionType as LdkTransactionType;
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::msgs::DecodeError;
use lightning::ln::types::ChannelId;
use lightning::offers::offer::OfferId;
use lightning::util::logger::Logger as _;
use lightning::util::ser::{Readable, Writeable};
use lightning::{
	_init_and_read_len_prefixed_tlv_fields, impl_writeable_tlv_based,
	impl_writeable_tlv_based_enum, write_tlv_fields,
};
use lightning_types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning_types::string::UntrustedString;

use crate::data_store::{DataStore, StorableObject, StorableObjectId, StorableObjectUpdate};
use crate::hex_utils;
use crate::logger::{log_debug, log_error, Logger};
use crate::Error;
use crate::UserChannelId;

/// Represents a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
				PaymentKind::Bolt11 { hash, preimage, secret, counterparty_skimmed_fee_msat: None }
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

	fn update(&mut self, update: Self::Update) -> bool {
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
				PaymentKind::Bolt11 { ref mut counterparty_skimmed_fee_msat, .. } => {
					update_if_necessary!(*counterparty_skimmed_fee_msat, skimmed_fee_msat);
				},
				_ => debug_assert!(
					false,
					"We should only ever override counterparty_skimmed_fee_msat for BOLT11 payments"
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

		if let Some(tx_id) = update.txid {
			match self.kind {
				PaymentKind::Onchain { ref mut txid, .. } => {
					update_if_necessary!(*txid, tx_id);
				},
				_ => {},
			}
		}

		if let Some(tx_type_update) = update.tx_type {
			match self.kind {
				PaymentKind::Onchain { ref mut tx_type, .. } => {
					update_if_necessary!(*tx_type, tx_type_update);
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

/// A channel referenced by a [`TransactionType`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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

/// The classification of a [`PaymentKind::Onchain`] transaction, as reported by LDK when the
/// transaction was broadcast.
///
/// Mirrors [`lightning::chain::chaininterface::TransactionType`], retaining the channel references
/// but dropping the broadcast-time contribution data; a transaction's amount and fee are tracked on
/// the [`PaymentDetails`] itself.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum TransactionType {
	/// A funding transaction establishing one or more new channels.
	Funding {
		/// The channels being funded.
		channels: Vec<Channel>,
	},
	/// A transaction cooperatively closing a channel.
	CooperativeClose {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel being closed.
		channel_id: ChannelId,
	},
	/// A transaction force-closing a channel.
	UnilateralClose {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel being force-closed.
		channel_id: ChannelId,
	},
	/// An anchor transaction CPFP fee-bumping a closing transaction.
	AnchorBump {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel whose closing transaction is being fee-bumped.
		channel_id: ChannelId,
	},
	/// A transaction resolving an output spendable by both us and our counterparty.
	Claim {
		/// The `node_id` of the channel counterparty.
		counterparty_node_id: PublicKey,
		/// The ID of the channel from which outputs are being claimed.
		channel_id: ChannelId,
	},
	/// A transaction sweeping spendable outputs to the on-chain wallet.
	Sweep {
		/// The channels from which outputs are being swept, if known.
		channels: Vec<Channel>,
	},
	/// An interactively-negotiated funding transaction: a splice, or (once supported) a V2
	/// dual-funded channel open.
	InteractiveFunding {
		/// The channels participating in the negotiation.
		channels: Vec<Channel>,
	},
}

impl_writeable_tlv_based_enum!(TransactionType,
	(0, Funding) => {
		(0, channels, optional_vec),
	},
	(2, CooperativeClose) => {
		(0, counterparty_node_id, required),
		(2, channel_id, required),
	},
	(4, UnilateralClose) => {
		(0, counterparty_node_id, required),
		(2, channel_id, required),
	},
	(6, AnchorBump) => {
		(0, counterparty_node_id, required),
		(2, channel_id, required),
	},
	(8, Claim) => {
		(0, counterparty_node_id, required),
		(2, channel_id, required),
	},
	(10, Sweep) => {
		(0, channels, optional_vec),
	},
	(12, InteractiveFunding) => {
		(0, channels, optional_vec),
	}
);

impl From<LdkTransactionType> for TransactionType {
	fn from(tx_type: LdkTransactionType) -> Self {
		let to_channels = |channels: Vec<(PublicKey, ChannelId)>| -> Vec<Channel> {
			channels
				.into_iter()
				.map(|(counterparty_node_id, channel_id)| Channel {
					counterparty_node_id,
					channel_id,
				})
				.collect()
		};
		match tx_type {
			LdkTransactionType::Funding { channels } => {
				TransactionType::Funding { channels: to_channels(channels) }
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
			LdkTransactionType::Sweep { channels } => {
				TransactionType::Sweep { channels: to_channels(channels) }
			},
			LdkTransactionType::InteractiveFunding { candidates } => {
				// Every candidate (the original negotiation plus any RBF replacements) references
				// the same channel(s); take the active (last) candidate's channel references.
				let channels = candidates
					.last()
					.map(|candidate| {
						candidate
							.channels
							.iter()
							.map(|cf| Channel {
								counterparty_node_id: cf.counterparty_node_id,
								channel_id: cf.channel_id,
							})
							.collect()
					})
					.unwrap_or_default();
				TransactionType::InteractiveFunding { channels }
			},
		}
	}
}

/// Represents the kind of a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
		/// The classification of this transaction, if known.
		///
		/// `None` for plain on-chain sends, and for records written by versions of LDK Node that
		/// predate on-chain transaction classification.
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
		/// The value, in thousands of a satoshi, that was deducted from this payment as an extra
		/// fee taken by our channel counterparty.
		///
		/// Will only ever be `Some` for inbound payments received via an [bLIP-52 / LSPS 2]
		/// just-in-time channel, and only after the payment is observed; `None` otherwise.
		///
		/// [bLIP-52 / LSPS 2]: https://github.com/lightning/blips/blob/master/blip-0052.md
		counterparty_skimmed_fee_msat: Option<u64>,
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
		(1, counterparty_skimmed_fee_msat, option),
		(2, preimage, option),
		(4, secret, option),
	},
	(4, Bolt11) => {
		(0, hash, required),
		(1, counterparty_skimmed_fee_msat, option),
		(2, preimage, option),
		(4, secret, option),
		(6, _legacy_lsps2_parameters, (legacy, LSPS2Parameters,
			|_| Ok(()),
			|_: &PaymentKind| None::<Option<LSPS2Parameters>>
		)),
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LSPS2Parameters {
	/// The maximal total amount we allow any configured LSP withhold from us when forwarding the
	/// payment.
	pub max_total_opening_fee_msat: Option<u64>,
	/// The maximal proportional fee, in parts-per-million millisatoshi, we allow any configured
	/// LSP withhold from us when forwarding the payment.
	pub max_proportional_opening_fee_ppm_msat: Option<u64>,
}

impl_writeable_tlv_based!(LSPS2Parameters, {
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
	pub txid: Option<Txid>,
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
			txid: None,
			tx_type: None,
		}
	}
}

impl From<&PaymentDetails> for PaymentDetailsUpdate {
	fn from(value: &PaymentDetails) -> Self {
		let (hash, preimage, secret) = match value.kind {
			PaymentKind::Bolt11 { hash, preimage, secret, .. } => (Some(hash), preimage, secret),
			PaymentKind::Bolt12Offer { hash, preimage, secret, .. } => (hash, preimage, secret),
			PaymentKind::Bolt12Refund { hash, preimage, secret, .. } => (hash, preimage, secret),
			PaymentKind::Spontaneous { hash, preimage, .. } => (Some(hash), preimage, None),
			_ => (None, None, None),
		};

		let (confirmation_status, txid, tx_type) = match &value.kind {
			PaymentKind::Onchain { status, txid, tx_type } => {
				(Some(*status), Some(*txid), Some(tx_type.clone()))
			},
			_ => (None, None, None),
		};

		let counterparty_skimmed_fee_msat = match value.kind {
			PaymentKind::Bolt11 { counterparty_skimmed_fee_msat, .. } => {
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
			txid,
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
	use lightning::util::ser::{Readable, Writeable};

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
	}

	impl_writeable_tlv_based!(OldPaymentDetails, {
		(0, hash, required),
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
				PaymentKind::Bolt11 {
					hash: h,
					preimage: p,
					secret: s,
					counterparty_skimmed_fee_msat: c,
				} => {
					assert_eq!(hash, h);
					assert_eq!(preimage, p);
					assert_eq!(secret, s);
					assert_eq!(None, c);
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

	#[derive(Clone, Debug, PartialEq, Eq)]
	struct OldOnchainKind {
		txid: Txid,
		status: ConfirmationStatus,
	}

	impl_writeable_tlv_based!(OldOnchainKind, {
		(0, txid, required),
		(2, status, required),
	});

	#[test]
	fn onchain_tx_type_deser_compat() {
		use bitcoin::hashes::Hash;
		use std::str::FromStr;

		let txid = Txid::from_byte_array([7u8; 32]);
		let status = ConfirmationStatus::Unconfirmed;

		// An `Onchain` record written before `tx_type` existed (only txid + status) must read back
		// with `tx_type: None`.
		let old = OldOnchainKind { txid, status };
		let mut on_disk = Vec::new();
		0u8.write(&mut on_disk).unwrap(); // the `Onchain` enum discriminant
		on_disk.extend_from_slice(&old.encode());
		match PaymentKind::read(&mut &*on_disk).unwrap() {
			PaymentKind::Onchain { txid: t, status: s, tx_type } => {
				assert_eq!(t, txid);
				assert_eq!(s, status);
				assert_eq!(tx_type, None);
			},
			other => panic!("Unexpected kind: {:?}", other),
		}

		// A populated `tx_type` round-trips.
		let kind = PaymentKind::Onchain {
			txid,
			status,
			tx_type: Some(TransactionType::InteractiveFunding {
				channels: vec![Channel {
					counterparty_node_id: PublicKey::from_str(
						"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
					)
					.unwrap(),
					channel_id: ChannelId([3u8; 32]),
				}],
			}),
		};
		assert_eq!(kind, PaymentKind::read(&mut &*kind.encode()).unwrap());
	}

	#[derive(Clone, Debug, PartialEq, Eq)]
	struct LegacyBolt11JitKind {
		hash: PaymentHash,
		counterparty_skimmed_fee_msat: Option<u64>,
		preimage: Option<PaymentPreimage>,
		secret: Option<PaymentSecret>,
		lsp_fee_limits: LSPS2Parameters,
	}

	impl_writeable_tlv_based!(LegacyBolt11JitKind, {
		(0, hash, required),
		(1, counterparty_skimmed_fee_msat, option),
		(2, preimage, option),
		(4, secret, option),
		(6, lsp_fee_limits, required),
	});

	#[test]
	fn legacy_bolt11_jit_kind_decodes_as_bolt11() {
		let hash = PaymentHash([42u8; 32]);
		let preimage = Some(PaymentPreimage([43u8; 32]));
		let secret = Some(PaymentSecret([44u8; 32]));
		let counterparty_skimmed_fee_msat = Some(7_777u64);
		let lsp_fee_limits = LSPS2Parameters {
			max_total_opening_fee_msat: Some(46_000),
			max_proportional_opening_fee_ppm_msat: Some(47_000),
		};

		let legacy = LegacyBolt11JitKind {
			hash,
			counterparty_skimmed_fee_msat,
			preimage,
			secret,
			lsp_fee_limits,
		};
		let legacy_encoded = legacy.encode();
		assert_eq!(legacy, LegacyBolt11JitKind::read(&mut &*legacy_encoded.clone()).unwrap());

		let mut on_disk = Vec::with_capacity(legacy_encoded.len() + 1);
		4u8.write(&mut on_disk).unwrap();
		on_disk.extend_from_slice(&legacy_encoded);

		let decoded = PaymentKind::read(&mut &*on_disk).unwrap();

		match decoded {
			PaymentKind::Bolt11 {
				hash: h,
				preimage: p,
				secret: s,
				counterparty_skimmed_fee_msat: c,
			} => {
				assert_eq!(hash, h);
				assert_eq!(preimage, p);
				assert_eq!(secret, s);
				assert_eq!(counterparty_skimmed_fee_msat, c);
			},
			other => panic!("Expected Bolt11, got {:?}", other),
		}

		let reencoded = decoded.encode();
		assert_eq!(reencoded[0], 2);
		assert_eq!(decoded, PaymentKind::read(&mut &*reencoded).unwrap());
	}
}

/// A unique identifier for a forwarded payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForwardedPaymentId(pub [u8; 32]);

impl StorableObjectId for ForwardedPaymentId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}

impl Writeable for ForwardedPaymentId {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		self.0.write(writer)
	}
}

impl Readable for ForwardedPaymentId {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		Ok(Self(Readable::read(reader)?))
	}
}

/// Details of a payment that has been forwarded through this node.
///
/// Detailed channel-pair records are only stored for forwards with exactly one incoming HTLC and
/// one outgoing HTLC. Multi-HTLC forwards are omitted from detailed channel-pair tracking until LDK
/// exposes enough data to identify the exact incoming/outgoing HTLC pairs.
///
/// TODO: Store multi-HTLC channel-pair records once LDK exposes exact pair data for trampoline
/// forwards.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ForwardedPaymentDetails {
	/// A randomly generated identifier for this forwarded payment.
	pub id: ForwardedPaymentId,
	/// The incoming channel id.
	pub prev_channel_id: ChannelId,
	/// The outgoing channel id.
	pub next_channel_id: ChannelId,
	/// The incoming user channel id, if available.
	pub prev_user_channel_id: Option<UserChannelId>,
	/// The outgoing user channel id, if available.
	pub next_user_channel_id: Option<UserChannelId>,
	/// The previous node id, if available.
	pub prev_node_id: Option<PublicKey>,
	/// The next node id, if available.
	pub next_node_id: Option<PublicKey>,
	/// The total fee earned by forwarding this payment, in millisatoshis.
	pub total_fee_earned_msat: Option<u64>,
	/// The skimmed fee, in millisatoshis.
	pub skimmed_fee_msat: Option<u64>,
	/// Whether the forwarded HTLC was claimed from an on-chain transaction.
	pub claim_from_onchain_tx: bool,
	/// The outbound amount forwarded, in millisatoshis.
	pub outbound_amount_forwarded_msat: Option<u64>,
	/// The timestamp when this payment was forwarded.
	pub forwarded_at_timestamp: u64,
}

impl_writeable_tlv_based!(ForwardedPaymentDetails, {
	(0, id, required),
	(2, prev_channel_id, required),
	(4, next_channel_id, required),
	(6, prev_user_channel_id, option),
	(8, next_user_channel_id, option),
	(10, prev_node_id, option),
	(12, next_node_id, option),
	(14, total_fee_earned_msat, option),
	(16, skimmed_fee_msat, option),
	(18, claim_from_onchain_tx, required),
	(20, outbound_amount_forwarded_msat, option),
	(22, forwarded_at_timestamp, required),
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ForwardedPaymentDetailsUpdate {
	id: ForwardedPaymentId,
}

impl StorableObjectUpdate<ForwardedPaymentDetails> for ForwardedPaymentDetailsUpdate {
	fn id(&self) -> ForwardedPaymentId {
		self.id
	}
}

impl StorableObject for ForwardedPaymentDetails {
	type Id = ForwardedPaymentId;
	type Update = ForwardedPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, _update: Self::Update) -> bool {
		false
	}

	fn to_update(&self) -> Self::Update {
		ForwardedPaymentDetailsUpdate { id: self.id }
	}
}

/// Aggregate statistics for forwarded payments through a single channel.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChannelForwardingStats {
	/// The channel id these stats apply to.
	pub channel_id: ChannelId,
	/// The channel counterparty node id, if known.
	pub counterparty_node_id: Option<PublicKey>,
	/// Number of forwarded payments where this was the incoming channel.
	pub inbound_payments_forwarded: u64,
	/// Number of forwarded payments where this was the outgoing channel.
	pub outbound_payments_forwarded: u64,
	/// Total inbound amount forwarded through this channel, in millisatoshis.
	pub total_inbound_amount_msat: u64,
	/// Total outbound amount forwarded through this channel, in millisatoshis.
	pub total_outbound_amount_msat: u64,
	/// Total forwarding fees earned through this channel, in millisatoshis.
	pub total_fee_earned_msat: u64,
	/// Total skimmed fees for this channel, in millisatoshis.
	pub total_skimmed_fee_msat: u64,
	/// Number of forwarded HTLCs claimed from on-chain transactions.
	pub onchain_claims_count: u64,
	/// Timestamp of the first forward recorded for this channel.
	pub first_forwarded_at_timestamp: u64,
	/// Timestamp of the latest forward recorded for this channel.
	pub last_forwarded_at_timestamp: u64,
}

impl_writeable_tlv_based!(ChannelForwardingStats, {
	(0, channel_id, required),
	(2, counterparty_node_id, option),
	(4, inbound_payments_forwarded, required),
	(6, outbound_payments_forwarded, required),
	(8, total_inbound_amount_msat, required),
	(10, total_outbound_amount_msat, required),
	(12, total_fee_earned_msat, required),
	(14, total_skimmed_fee_msat, required),
	(16, onchain_claims_count, required),
	(18, first_forwarded_at_timestamp, required),
	(20, last_forwarded_at_timestamp, required),
});

/// Channel pair identifier, formed from previous channel, next channel, and time bucket.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelPairStatsId(pub [u8; 72]);

impl ChannelPairStatsId {
	pub fn from_channel_pair_and_bucket(
		prev: &ChannelId, next: &ChannelId, bucket_start_timestamp: u64,
	) -> Self {
		let mut result = [0u8; 72];
		result[0..32].copy_from_slice(&prev.0);
		result[32..64].copy_from_slice(&next.0);
		result[64..72].copy_from_slice(&bucket_start_timestamp.to_be_bytes());
		Self(result)
	}
}

impl Writeable for ChannelPairStatsId {
	fn write<W: lightning::util::ser::Writer>(
		&self, writer: &mut W,
	) -> Result<(), lightning::io::Error> {
		writer.write_all(&self.0)
	}
}

impl Readable for ChannelPairStatsId {
	fn read<R: lightning::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let mut bytes = [0u8; 72];
		reader.read_exact(&mut bytes)?;
		Ok(Self(bytes))
	}
}

impl Display for ChannelPairStatsId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", hex_utils::to_string(&self.0))
	}
}

impl Debug for ChannelPairStatsId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "ChannelPairStatsId({})", hex_utils::to_string(&self.0))
	}
}

impl StorableObjectId for ChannelPairStatsId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}

/// Aggregated statistics for a specific channel pair.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChannelPairForwardingStats {
	/// The unique channel-pair bucket id.
	pub id: ChannelPairStatsId,
	/// The incoming channel id.
	pub prev_channel_id: ChannelId,
	/// The outgoing channel id.
	pub next_channel_id: ChannelId,
	/// Start timestamp of this aggregation bucket.
	pub bucket_start_timestamp: u64,
	/// The previous node id, if available.
	pub prev_node_id: Option<PublicKey>,
	/// The next node id, if available.
	pub next_node_id: Option<PublicKey>,
	/// Number of payments aggregated in this bucket.
	pub payment_count: u64,
	/// Total inbound amount in this bucket, in millisatoshis.
	pub total_inbound_amount_msat: u64,
	/// Total outbound amount in this bucket, in millisatoshis.
	pub total_outbound_amount_msat: u64,
	/// Total forwarding fees earned in this bucket, in millisatoshis.
	pub total_fee_earned_msat: u64,
	/// Total skimmed fees in this bucket, in millisatoshis.
	pub total_skimmed_fee_msat: u64,
	/// Number of forwarded HTLCs claimed from on-chain transactions.
	pub onchain_claims_count: u64,
	/// Average forwarding fee per payment, in millisatoshis.
	pub avg_fee_msat: u64,
	/// Average inbound amount per payment, in millisatoshis.
	pub avg_inbound_amount_msat: u64,
	/// Timestamp of the first forward in this bucket.
	pub first_forwarded_at_timestamp: u64,
	/// Timestamp of the latest forward in this bucket.
	pub last_forwarded_at_timestamp: u64,
	/// Timestamp when this bucket was aggregated.
	pub aggregated_at_timestamp: u64,
}

impl_writeable_tlv_based!(ChannelPairForwardingStats, {
	(0, id, required),
	(2, prev_channel_id, required),
	(4, next_channel_id, required),
	(6, prev_node_id, option),
	(8, next_node_id, option),
	(10, payment_count, required),
	(12, total_inbound_amount_msat, required),
	(14, total_outbound_amount_msat, required),
	(16, total_fee_earned_msat, required),
	(18, total_skimmed_fee_msat, required),
	(20, onchain_claims_count, required),
	(22, avg_fee_msat, required),
	(24, avg_inbound_amount_msat, required),
	(26, first_forwarded_at_timestamp, required),
	(28, last_forwarded_at_timestamp, required),
	(30, aggregated_at_timestamp, required),
	(32, bucket_start_timestamp, required),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelForwardingStatsUpdate {
	pub channel_id: ChannelId,
	pub counterparty_node_id: Option<PublicKey>,
	pub inbound_payments_increment: u64,
	pub outbound_payments_increment: u64,
	pub inbound_amount_increment_msat: u64,
	pub outbound_amount_increment_msat: u64,
	pub fee_earned_increment_msat: u64,
	pub skimmed_fee_increment_msat: u64,
	pub onchain_claims_increment: u64,
	pub timestamp: u64,
}

impl StorableObjectUpdate<ChannelForwardingStats> for ChannelForwardingStatsUpdate {
	fn id(&self) -> ChannelId {
		self.channel_id
	}
}

impl StorableObjectId for ChannelId {
	fn encode_to_hex_str(&self) -> String {
		hex_utils::to_string(&self.0)
	}
}

impl StorableObject for ChannelForwardingStats {
	type Id = ChannelId;
	type Update = ChannelForwardingStatsUpdate;

	fn id(&self) -> Self::Id {
		self.channel_id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		debug_assert_eq!(self.channel_id, update.channel_id);
		let mut updated = false;
		if self.counterparty_node_id.is_none() && update.counterparty_node_id.is_some() {
			self.counterparty_node_id = update.counterparty_node_id;
			updated = true;
		}
		if update.inbound_payments_increment > 0 {
			self.inbound_payments_forwarded += update.inbound_payments_increment;
			updated = true;
		}
		if update.outbound_payments_increment > 0 {
			self.outbound_payments_forwarded += update.outbound_payments_increment;
			updated = true;
		}
		if update.inbound_amount_increment_msat > 0 {
			self.total_inbound_amount_msat += update.inbound_amount_increment_msat;
			updated = true;
		}
		if update.outbound_amount_increment_msat > 0 {
			self.total_outbound_amount_msat += update.outbound_amount_increment_msat;
			updated = true;
		}
		if update.fee_earned_increment_msat > 0 {
			self.total_fee_earned_msat += update.fee_earned_increment_msat;
			updated = true;
		}
		if update.skimmed_fee_increment_msat > 0 {
			self.total_skimmed_fee_msat += update.skimmed_fee_increment_msat;
			updated = true;
		}
		if update.onchain_claims_increment > 0 {
			self.onchain_claims_count += update.onchain_claims_increment;
			updated = true;
		}
		if updated {
			self.first_forwarded_at_timestamp =
				self.first_forwarded_at_timestamp.min(update.timestamp);
			self.last_forwarded_at_timestamp =
				self.last_forwarded_at_timestamp.max(update.timestamp);
		}
		updated
	}

	fn to_update(&self) -> Self::Update {
		ChannelForwardingStatsUpdate {
			channel_id: self.channel_id,
			counterparty_node_id: self.counterparty_node_id,
			inbound_payments_increment: self.inbound_payments_forwarded,
			outbound_payments_increment: self.outbound_payments_forwarded,
			inbound_amount_increment_msat: self.total_inbound_amount_msat,
			outbound_amount_increment_msat: self.total_outbound_amount_msat,
			fee_earned_increment_msat: self.total_fee_earned_msat,
			skimmed_fee_increment_msat: self.total_skimmed_fee_msat,
			onchain_claims_increment: self.onchain_claims_count,
			timestamp: self.last_forwarded_at_timestamp,
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelPairForwardingStatsUpdate {
	pub id: ChannelPairStatsId,
	pub prev_node_id: Option<PublicKey>,
	pub next_node_id: Option<PublicKey>,
	pub payment_count_increment: u64,
	pub inbound_amount_increment_msat: u64,
	pub outbound_amount_increment_msat: u64,
	pub fee_earned_increment_msat: u64,
	pub skimmed_fee_increment_msat: u64,
	pub onchain_claims_increment: u64,
	pub first_timestamp: u64,
	pub last_timestamp: u64,
}

impl StorableObjectUpdate<ChannelPairForwardingStats> for ChannelPairForwardingStatsUpdate {
	fn id(&self) -> ChannelPairStatsId {
		self.id
	}
}

impl StorableObject for ChannelPairForwardingStats {
	type Id = ChannelPairStatsId;
	type Update = ChannelPairForwardingStatsUpdate;

	fn id(&self) -> Self::Id {
		self.id
	}

	fn update(&mut self, update: Self::Update) -> bool {
		debug_assert_eq!(self.id, update.id);
		let mut updated = false;
		if self.prev_node_id.is_none() && update.prev_node_id.is_some() {
			self.prev_node_id = update.prev_node_id;
			updated = true;
		}
		if self.next_node_id.is_none() && update.next_node_id.is_some() {
			self.next_node_id = update.next_node_id;
			updated = true;
		}
		if update.payment_count_increment > 0 {
			self.payment_count += update.payment_count_increment;
			updated = true;
		}
		if update.inbound_amount_increment_msat > 0 {
			self.total_inbound_amount_msat += update.inbound_amount_increment_msat;
			updated = true;
		}
		if update.outbound_amount_increment_msat > 0 {
			self.total_outbound_amount_msat += update.outbound_amount_increment_msat;
			updated = true;
		}
		if update.fee_earned_increment_msat > 0 {
			self.total_fee_earned_msat += update.fee_earned_increment_msat;
			updated = true;
		}
		if update.skimmed_fee_increment_msat > 0 {
			self.total_skimmed_fee_msat += update.skimmed_fee_increment_msat;
			updated = true;
		}
		if update.onchain_claims_increment > 0 {
			self.onchain_claims_count += update.onchain_claims_increment;
			updated = true;
		}
		if updated {
			if self.first_forwarded_at_timestamp == 0 {
				self.first_forwarded_at_timestamp = update.first_timestamp;
			} else {
				self.first_forwarded_at_timestamp =
					self.first_forwarded_at_timestamp.min(update.first_timestamp);
			}
			self.last_forwarded_at_timestamp =
				self.last_forwarded_at_timestamp.max(update.last_timestamp);
			if self.payment_count > 0 {
				self.avg_fee_msat = self.total_fee_earned_msat / self.payment_count;
				self.avg_inbound_amount_msat = self.total_inbound_amount_msat / self.payment_count;
			}
		}
		updated
	}

	fn to_update(&self) -> Self::Update {
		ChannelPairForwardingStatsUpdate {
			id: self.id,
			prev_node_id: self.prev_node_id,
			next_node_id: self.next_node_id,
			payment_count_increment: self.payment_count,
			inbound_amount_increment_msat: self.total_inbound_amount_msat,
			outbound_amount_increment_msat: self.total_outbound_amount_msat,
			fee_earned_increment_msat: self.total_fee_earned_msat,
			skimmed_fee_increment_msat: self.total_skimmed_fee_msat,
			onchain_claims_increment: self.onchain_claims_count,
			first_timestamp: self.first_forwarded_at_timestamp,
			last_timestamp: self.last_forwarded_at_timestamp,
		}
	}
}

/// Aggregate expired forwarded payments into time-bucketed channel pair statistics.
pub(crate) async fn aggregate_expired_forwarded_payments(
	forwarded_payment_store: &DataStore<ForwardedPaymentDetails, Arc<Logger>>,
	channel_pair_stats_store: &DataStore<ChannelPairForwardingStats, Arc<Logger>>,
	retention_minutes: u64, logger: &Arc<Logger>,
) -> Result<(u64, u64), Error> {
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	let cutoff = now.saturating_sub(retention_minutes.saturating_mul(60));
	let bucket_size_secs = retention_minutes.saturating_mul(60);
	if bucket_size_secs == 0 {
		return Ok((0, 0));
	}

	let expired = forwarded_payment_store.list_filter(|p| p.forwarded_at_timestamp < cutoff);
	if expired.is_empty() {
		log_debug!(logger, "No expired forwarded payments found");
		return Ok((0, 0));
	}

	let mut bucket_groups: HashMap<(ChannelId, ChannelId, u64), Vec<&ForwardedPaymentDetails>> =
		HashMap::new();
	for payment in &expired {
		let bucket_start = (payment.forwarded_at_timestamp / bucket_size_secs) * bucket_size_secs;
		bucket_groups
			.entry((payment.prev_channel_id, payment.next_channel_id, bucket_start))
			.or_default()
			.push(payment);
	}

	let mut aggregated_bucket_count = 0u64;
	let mut removed_payment_count = 0u64;
	for ((prev_channel_id, next_channel_id, bucket_start), payments) in bucket_groups {
		let pair_id = ChannelPairStatsId::from_channel_pair_and_bucket(
			&prev_channel_id,
			&next_channel_id,
			bucket_start,
		);

		let first_payment = payments[0];
		let mut total_inbound_amount_msat = 0u64;
		let mut total_outbound_amount_msat = 0u64;
		let mut total_fee_earned_msat = 0u64;
		let mut total_skimmed_fee_msat = 0u64;
		let mut onchain_claims_count = 0u64;
		let mut first_timestamp = u64::MAX;
		let mut last_timestamp = 0u64;

		for payment in &payments {
			let outbound = payment.outbound_amount_forwarded_msat.unwrap_or(0);
			let fee = payment.total_fee_earned_msat.unwrap_or(0);
			let skimmed = payment.skimmed_fee_msat.unwrap_or(0);
			total_inbound_amount_msat =
				total_inbound_amount_msat.saturating_add(outbound.saturating_add(fee));
			total_outbound_amount_msat = total_outbound_amount_msat.saturating_add(outbound);
			total_fee_earned_msat = total_fee_earned_msat.saturating_add(fee);
			total_skimmed_fee_msat = total_skimmed_fee_msat.saturating_add(skimmed);
			if payment.claim_from_onchain_tx {
				onchain_claims_count += 1;
			}
			first_timestamp = first_timestamp.min(payment.forwarded_at_timestamp);
			last_timestamp = last_timestamp.max(payment.forwarded_at_timestamp);
		}

		let payment_count = payments.len() as u64;
		let stats = ChannelPairForwardingStats {
			id: pair_id,
			prev_channel_id,
			next_channel_id,
			bucket_start_timestamp: bucket_start,
			prev_node_id: first_payment.prev_node_id,
			next_node_id: first_payment.next_node_id,
			payment_count,
			total_inbound_amount_msat,
			total_outbound_amount_msat,
			total_fee_earned_msat,
			total_skimmed_fee_msat,
			onchain_claims_count,
			avg_fee_msat: total_fee_earned_msat / payment_count,
			avg_inbound_amount_msat: total_inbound_amount_msat / payment_count,
			first_forwarded_at_timestamp: first_timestamp,
			last_forwarded_at_timestamp: last_timestamp,
			aggregated_at_timestamp: now,
		};

		channel_pair_stats_store.insert_or_update(stats).await.map_err(|e| {
			log_error!(logger, "Failed to upsert channel pair stats bucket for {pair_id:?}: {e}");
			e
		})?;
		aggregated_bucket_count += 1;

		for payment in payments {
			forwarded_payment_store.remove(&payment.id).await.map_err(|e| {
				log_error!(logger, "Failed to remove forwarded payment {:?}: {}", payment.id, e);
				e
			})?;
			removed_payment_count += 1;
		}
	}

	Ok((aggregated_bucket_count, removed_payment_count))
}

/// Aggregates multiple channel pair statistics buckets into cumulative totals.
pub fn aggregate_channel_pair_stats(
	buckets: &[ChannelPairForwardingStats],
) -> ChannelPairForwardingStats {
	assert!(!buckets.is_empty(), "Cannot aggregate empty bucket list");
	let first = &buckets[0];
	for bucket in &buckets[1..] {
		assert_eq!(bucket.prev_channel_id, first.prev_channel_id);
		assert_eq!(bucket.next_channel_id, first.next_channel_id);
	}

	let mut payment_count = 0u64;
	let mut total_inbound_amount_msat = 0u64;
	let mut total_outbound_amount_msat = 0u64;
	let mut total_fee_earned_msat = 0u64;
	let mut total_skimmed_fee_msat = 0u64;
	let mut onchain_claims_count = 0u64;
	let mut first_forwarded_at_timestamp = u64::MAX;
	let mut last_forwarded_at_timestamp = 0u64;
	let mut earliest_bucket_start = u64::MAX;
	for bucket in buckets {
		payment_count += bucket.payment_count;
		total_inbound_amount_msat += bucket.total_inbound_amount_msat;
		total_outbound_amount_msat += bucket.total_outbound_amount_msat;
		total_fee_earned_msat += bucket.total_fee_earned_msat;
		total_skimmed_fee_msat += bucket.total_skimmed_fee_msat;
		onchain_claims_count += bucket.onchain_claims_count;
		first_forwarded_at_timestamp =
			first_forwarded_at_timestamp.min(bucket.first_forwarded_at_timestamp);
		last_forwarded_at_timestamp =
			last_forwarded_at_timestamp.max(bucket.last_forwarded_at_timestamp);
		earliest_bucket_start = earliest_bucket_start.min(bucket.bucket_start_timestamp);
	}
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	ChannelPairForwardingStats {
		id: ChannelPairStatsId::from_channel_pair_and_bucket(
			&first.prev_channel_id,
			&first.next_channel_id,
			earliest_bucket_start,
		),
		prev_channel_id: first.prev_channel_id,
		next_channel_id: first.next_channel_id,
		bucket_start_timestamp: earliest_bucket_start,
		prev_node_id: first.prev_node_id,
		next_node_id: first.next_node_id,
		payment_count,
		total_inbound_amount_msat,
		total_outbound_amount_msat,
		total_fee_earned_msat,
		total_skimmed_fee_msat,
		onchain_claims_count,
		avg_fee_msat: if payment_count > 0 { total_fee_earned_msat / payment_count } else { 0 },
		avg_inbound_amount_msat: if payment_count > 0 {
			total_inbound_amount_msat / payment_count
		} else {
			0
		},
		first_forwarded_at_timestamp,
		last_forwarded_at_timestamp,
		aggregated_at_timestamp: now,
	}
}
