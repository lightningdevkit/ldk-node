// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use base64::Engine;
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

pub(crate) const FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS: u64 = 60 * 60;

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
	fn encode_to_key(&self) -> String {
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
					if tx_type.is_none() || tx_type_update.is_some() {
						update_if_necessary!(*tx_type, tx_type_update);
					}
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
		use std::str::FromStr;

		use bitcoin::hashes::Hash;

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

	#[test]
	fn known_onchain_tx_type_survives_unknown_update() {
		use std::str::FromStr;

		use bitcoin::hashes::Hash;

		let txid = Txid::from_byte_array([8u8; 32]);
		let payment_id = PaymentId(txid.to_byte_array());
		let pubkey = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		let tx_type = TransactionType::CooperativeClose {
			counterparty_node_id: pubkey,
			channel_id: ChannelId([4u8; 32]),
		};
		let mut classified = PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Unconfirmed,
				tx_type: Some(tx_type.clone()),
			},
			Some(1_000),
			Some(100),
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);
		let wallet_sync_update = PaymentDetails::new(
			payment_id,
			PaymentKind::Onchain {
				txid,
				status: ConfirmationStatus::Confirmed {
					block_hash: BlockHash::from_byte_array([9u8; 32]),
					height: 42,
					timestamp: 123,
				},
				tx_type: None,
			},
			Some(1_000),
			Some(100),
			PaymentDirection::Inbound,
			PaymentStatus::Pending,
		);

		assert!(classified.update(PaymentDetailsUpdate::from(&wallet_sync_update)));
		match classified.kind {
			PaymentKind::Onchain { status, tx_type: Some(updated_tx_type), .. } => {
				assert!(matches!(status, ConfirmationStatus::Confirmed { height: 42, .. }));
				assert_eq!(updated_tx_type, tx_type);
			},
			other => panic!("Unexpected payment kind: {:?}", other),
		}
	}

	#[test]
	fn transaction_type_from_ldk_variants() {
		use std::str::FromStr;

		let pubkey = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		let channel_id = ChannelId([5u8; 32]);
		let channel = Channel { counterparty_node_id: pubkey, channel_id };

		let variants = vec![
			(
				LdkTransactionType::Funding { channels: vec![(pubkey, channel_id)] },
				TransactionType::Funding { channels: vec![channel.clone()] },
			),
			(
				LdkTransactionType::CooperativeClose { counterparty_node_id: pubkey, channel_id },
				TransactionType::CooperativeClose { counterparty_node_id: pubkey, channel_id },
			),
			(
				LdkTransactionType::UnilateralClose { counterparty_node_id: pubkey, channel_id },
				TransactionType::UnilateralClose { counterparty_node_id: pubkey, channel_id },
			),
			(
				LdkTransactionType::AnchorBump { counterparty_node_id: pubkey, channel_id },
				TransactionType::AnchorBump { counterparty_node_id: pubkey, channel_id },
			),
			(
				LdkTransactionType::Claim { counterparty_node_id: pubkey, channel_id },
				TransactionType::Claim { counterparty_node_id: pubkey, channel_id },
			),
			(
				LdkTransactionType::Sweep { channels: vec![(pubkey, channel_id)] },
				TransactionType::Sweep { channels: vec![channel] },
			),
		];

		for (ldk_type, expected_type) in variants {
			assert_eq!(TransactionType::from(ldk_type), expected_type);
		}
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

/// Details of a payment that has been forwarded through this node.
///
/// For multi-HTLC forwards, records use FIFO bookkeeping to divide the reported amounts between
/// incoming and outgoing channel pairs. The incoming and outgoing HTLCs are separate batches, so
/// FIFO defines an accounting allocation between them. It preserves the event totals, but the
/// allocation can depend on the order of the HTLCs.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ForwardedPaymentDetails {
	/// An opaque, randomly generated identifier for this forwarded payment.
	pub id: String,
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
	/// The inbound amount attributed to this channel pair, in millisatoshis.
	pub inbound_amount_forwarded_msat: Option<u64>,
	/// The fee attributed to this channel pair, in millisatoshis.
	pub total_fee_earned_msat: Option<u64>,
	/// The skimmed fee attributed to this channel pair, in millisatoshis.
	pub skimmed_fee_msat: Option<u64>,
	/// Whether the forwarded HTLC was claimed from an on-chain transaction.
	pub claim_from_onchain_tx: bool,
	/// The outbound amount attributed to this channel pair, in millisatoshis.
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
	(24, inbound_amount_forwarded_msat, option),
});

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ForwardedPaymentDetailsUpdate {
	id: String,
}

impl StorableObjectUpdate<ForwardedPaymentDetails> for ForwardedPaymentDetailsUpdate {
	fn id(&self) -> String {
		self.id.clone()
	}
}

impl StorableObject for ForwardedPaymentDetails {
	type Id = String;
	type Update = ForwardedPaymentDetailsUpdate;

	fn id(&self) -> Self::Id {
		self.id.clone()
	}

	fn update(&mut self, _update: Self::Update) -> bool {
		false
	}

	fn to_update(&self) -> Self::Update {
		ForwardedPaymentDetailsUpdate { id: self.id.clone() }
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
	/// Total forwarding fees earned through this channel, in millisatoshis, if known for every
	/// recorded forward.
	pub total_fee_earned_msat: Option<u64>,
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
	(12, total_fee_earned_msat, option),
	(14, total_skimmed_fee_msat, required),
	(16, onchain_claims_count, required),
	(18, first_forwarded_at_timestamp, required),
	(20, last_forwarded_at_timestamp, required),
});

fn channel_pair_stats_id(
	prev: &ChannelId, next: &ChannelId, bucket_start_timestamp: u64, bucket_size_secs: u64,
) -> String {
	let mut bytes = [0u8; 80];
	bytes[0..32].copy_from_slice(&prev.0);
	bytes[32..64].copy_from_slice(&next.0);
	bytes[64..72].copy_from_slice(&bucket_start_timestamp.to_be_bytes());
	bytes[72..80].copy_from_slice(&bucket_size_secs.to_be_bytes());
	// The full ID is 160 hex characters, exceeding LDK's 120-character KVStore key limit.
	BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

/// Aggregated statistics for a specific channel pair.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ChannelPairForwardingStats {
	/// An opaque identifier for this channel-pair bucket.
	pub id: String,
	/// The incoming channel id.
	pub prev_channel_id: ChannelId,
	/// The outgoing channel id.
	pub next_channel_id: ChannelId,
	/// Start timestamp of this aggregation bucket.
	pub bucket_start_timestamp: u64,
	/// Width of this aggregation bucket, in seconds.
	pub bucket_size_secs: u64,
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
	/// Total forwarding fees earned in this bucket, in millisatoshis, if known for every payment.
	pub total_fee_earned_msat: Option<u64>,
	/// Total skimmed fees in this bucket, in millisatoshis.
	pub total_skimmed_fee_msat: u64,
	/// Number of forwarded HTLCs claimed from on-chain transactions.
	pub onchain_claims_count: u64,
	/// Average forwarding fee per payment, in millisatoshis, if known for every payment.
	pub avg_fee_msat: Option<u64>,
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
	(16, total_fee_earned_msat, option),
	(18, total_skimmed_fee_msat, required),
	(20, onchain_claims_count, required),
	(22, avg_fee_msat, option),
	(24, avg_inbound_amount_msat, required),
	(26, first_forwarded_at_timestamp, required),
	(28, last_forwarded_at_timestamp, required),
	(30, aggregated_at_timestamp, required),
	(32, bucket_start_timestamp, required),
	(34, bucket_size_secs, required),
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelForwardingStatsUpdate {
	pub channel_id: ChannelId,
	pub counterparty_node_id: Option<PublicKey>,
	pub inbound_payments_increment: u64,
	pub outbound_payments_increment: u64,
	pub inbound_amount_increment_msat: u64,
	pub outbound_amount_increment_msat: u64,
	pub fee_earned_increment_msat: Option<u64>,
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
	fn encode_to_key(&self) -> String {
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
		match (self.total_fee_earned_msat.as_mut(), update.fee_earned_increment_msat) {
			(Some(total), Some(increment)) if increment > 0 => {
				*total += increment;
				updated = true;
			},
			(Some(_), None) => {
				self.total_fee_earned_msat = None;
				updated = true;
			},
			_ => {},
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
	pub id: String,
	pub prev_node_id: Option<PublicKey>,
	pub next_node_id: Option<PublicKey>,
	pub payment_count_increment: u64,
	pub inbound_amount_increment_msat: u64,
	pub outbound_amount_increment_msat: u64,
	pub fee_earned_increment_msat: Option<u64>,
	pub skimmed_fee_increment_msat: u64,
	pub onchain_claims_increment: u64,
	pub first_timestamp: u64,
	pub last_timestamp: u64,
	pub aggregated_at_timestamp: u64,
}

impl StorableObjectUpdate<ChannelPairForwardingStats> for ChannelPairForwardingStatsUpdate {
	fn id(&self) -> String {
		self.id.clone()
	}
}

impl StorableObject for ChannelPairForwardingStats {
	type Id = String;
	type Update = ChannelPairForwardingStatsUpdate;

	fn id(&self) -> Self::Id {
		self.id.clone()
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
		match (self.total_fee_earned_msat.as_mut(), update.fee_earned_increment_msat) {
			(Some(total), Some(increment)) if increment > 0 => {
				*total += increment;
				updated = true;
			},
			(Some(_), None) => {
				self.total_fee_earned_msat = None;
				updated = true;
			},
			_ => {},
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
				self.avg_fee_msat =
					self.total_fee_earned_msat.map(|total| total / self.payment_count);
				self.avg_inbound_amount_msat = self.total_inbound_amount_msat / self.payment_count;
			}
		}
		if update.aggregated_at_timestamp > self.aggregated_at_timestamp {
			self.aggregated_at_timestamp = update.aggregated_at_timestamp;
			updated = true;
		}
		updated
	}

	fn to_update(&self) -> Self::Update {
		ChannelPairForwardingStatsUpdate {
			id: self.id.clone(),
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
			aggregated_at_timestamp: self.aggregated_at_timestamp,
		}
	}
}

fn seconds_until_next_forwarding_aggregation(now_timestamp: u64, bucket_size_secs: u64) -> u64 {
	debug_assert!(bucket_size_secs > 0);
	bucket_size_secs - (now_timestamp % bucket_size_secs)
}

pub(crate) async fn run_forwarded_payment_aggregation(
	mut stop_receiver: tokio::sync::watch::Receiver<()>,
	forwarded_payment_store: Arc<DataStore<ForwardedPaymentDetails, Arc<Logger>>>,
	channel_pair_stats_store: Arc<DataStore<ChannelPairForwardingStats, Arc<Logger>>>,
	retention_secs: u64, logger: Arc<Logger>,
) {
	let period = Duration::from_secs(FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS);
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	let secs_until_next_bucket = seconds_until_next_forwarding_aggregation(
		now,
		FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS,
	);
	let first_tick = tokio::time::Instant::now() + Duration::from_secs(secs_until_next_bucket);
	let mut interval = tokio::time::interval_at(first_tick, period);
	interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	if retention_secs == 0 && forwarded_payment_store.is_empty() {
		return;
	}
	loop {
		tokio::select! {
			_ = stop_receiver.changed() => break,
			_ = interval.tick() => {
				match aggregate_expired_forwarded_payments(
					&forwarded_payment_store,
					&channel_pair_stats_store,
					retention_secs,
					&logger,
				)
				.await
				{
					Ok((pair_count, payment_count)) if pair_count > 0 => {
						log_debug!(
							logger,
							"Aggregated {} forwarded payments into {} channel pair buckets",
							payment_count,
							pair_count
						);
					},
					Ok((0, payment_count)) if payment_count > 0 => {
						log_debug!(
							logger,
							"Removed {} forwarded payment details from previously aggregated buckets",
							payment_count
						);
					},
					Err(e) => log_error!(
						logger,
						"Periodic forwarded payment aggregation failed: {}",
						e
					),
					_ => {},
				}
				if retention_secs == 0 && forwarded_payment_store.is_empty() {
					break;
				}
			}
		}
	}
}

/// Aggregate forwarded payments older than the configured retention period into fixed-width
/// channel-pair statistics buckets.
pub(crate) async fn aggregate_expired_forwarded_payments(
	forwarded_payment_store: &DataStore<ForwardedPaymentDetails, Arc<Logger>>,
	channel_pair_stats_store: &DataStore<ChannelPairForwardingStats, Arc<Logger>>,
	retention_secs: u64, logger: &Arc<Logger>,
) -> Result<(u64, u64), Error> {
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	aggregate_expired_forwarded_payments_at(
		forwarded_payment_store,
		channel_pair_stats_store,
		FORWARDED_PAYMENT_AGGREGATION_BUCKET_SIZE_SECS,
		retention_secs,
		now,
		logger,
	)
	.await
}

async fn aggregate_expired_forwarded_payments_at(
	forwarded_payment_store: &DataStore<ForwardedPaymentDetails, Arc<Logger>>,
	channel_pair_stats_store: &DataStore<ChannelPairForwardingStats, Arc<Logger>>,
	bucket_size_secs: u64, retention_secs: u64, now: u64, logger: &Arc<Logger>,
) -> Result<(u64, u64), Error> {
	if bucket_size_secs == 0 {
		return Ok((0, 0));
	}
	let retention_cutoff = now.saturating_sub(retention_secs);
	let oldest_retained_bucket_start =
		(retention_cutoff / bucket_size_secs).saturating_mul(bucket_size_secs);

	// Retain details for at least the configured period. We only aggregate complete buckets, so
	// details may remain for up to one additional bucket width. Once an older bucket is persisted,
	// it is a durable commit marker: retries can skip updating its totals and finish deleting detail
	// records left behind by an interrupted cleanup.
	// Detail insertions assign their timestamps while holding this same lock. Holding it until all
	// closed-bucket markers are persisted ensures a record can either be included in the marker or
	// receive a timestamp after the aggregation pass, but cannot be inserted late into a committed
	// bucket.
	let forwarded_payment_guard = forwarded_payment_store.mutation_guard().await;
	let expired = forwarded_payment_store
		.list_filter(|p| p.forwarded_at_timestamp < oldest_retained_bucket_start);
	if expired.is_empty() {
		log_debug!(logger, "No forwarded payments in closed aggregation buckets found");
		return Ok((0, 0));
	}

	let mut bucket_groups: HashMap<(ChannelId, ChannelId, u64), Vec<ForwardedPaymentDetails>> =
		HashMap::new();
	for payment in expired {
		let bucket_start = (payment.forwarded_at_timestamp / bucket_size_secs) * bucket_size_secs;
		bucket_groups
			.entry((payment.prev_channel_id, payment.next_channel_id, bucket_start))
			.or_default()
			.push(payment);
	}

	let mut aggregated_bucket_count = 0u64;
	let mut payment_ids_to_remove = Vec::new();
	for ((prev_channel_id, next_channel_id, bucket_start), payments) in bucket_groups {
		let pair_id = channel_pair_stats_id(
			&prev_channel_id,
			&next_channel_id,
			bucket_start,
			bucket_size_secs,
		);

		if !channel_pair_stats_store.contains_key(&pair_id) {
			let mut total_inbound_amount_msat = 0u64;
			let mut total_outbound_amount_msat = 0u64;
			let mut total_fee_earned_msat = Some(0u64);
			let mut total_skimmed_fee_msat = 0u64;
			let mut onchain_claims_count = 0u64;
			let mut first_timestamp = u64::MAX;
			let mut last_timestamp = 0u64;

			for payment in &payments {
				let outbound = payment.outbound_amount_forwarded_msat.unwrap_or(0);
				let fee = payment.total_fee_earned_msat;
				let skimmed = payment.skimmed_fee_msat.unwrap_or(0);
				let inbound = payment
					.inbound_amount_forwarded_msat
					.unwrap_or_else(|| outbound.saturating_add(fee.unwrap_or(0)));
				total_inbound_amount_msat = total_inbound_amount_msat.saturating_add(inbound);
				total_outbound_amount_msat = total_outbound_amount_msat.saturating_add(outbound);
				total_fee_earned_msat = match (total_fee_earned_msat, fee) {
					(Some(total), Some(fee)) => Some(total.saturating_add(fee)),
					_ => None,
				};
				total_skimmed_fee_msat = total_skimmed_fee_msat.saturating_add(skimmed);
				if payment.claim_from_onchain_tx {
					onchain_claims_count += 1;
				}
				first_timestamp = first_timestamp.min(payment.forwarded_at_timestamp);
				last_timestamp = last_timestamp.max(payment.forwarded_at_timestamp);
			}

			let payment_count = payments.len() as u64;
			let prev_node_id = payments.iter().find_map(|payment| payment.prev_node_id);
			let next_node_id = payments.iter().find_map(|payment| payment.next_node_id);
			let stats = ChannelPairForwardingStats {
				id: pair_id.clone(),
				prev_channel_id,
				next_channel_id,
				bucket_start_timestamp: bucket_start,
				bucket_size_secs,
				prev_node_id,
				next_node_id,
				payment_count,
				total_inbound_amount_msat,
				total_outbound_amount_msat,
				total_fee_earned_msat,
				total_skimmed_fee_msat,
				onchain_claims_count,
				avg_fee_msat: total_fee_earned_msat.map(|total| total / payment_count),
				avg_inbound_amount_msat: total_inbound_amount_msat / payment_count,
				first_forwarded_at_timestamp: first_timestamp,
				last_forwarded_at_timestamp: last_timestamp,
				aggregated_at_timestamp: now,
			};

			channel_pair_stats_store.insert(stats).await.map_err(|e| {
				log_error!(
					logger,
					"Failed to insert channel pair stats bucket for {pair_id:?}: {e}"
				);
				e
			})?;
			aggregated_bucket_count += 1;
		}

		payment_ids_to_remove.extend(payments.into_iter().map(|payment| payment.id));
	}

	// Removals acquire the mutation lock themselves. Once every bucket marker has been persisted,
	// detail insertions may resume without risking insertion into any bucket closed by this pass.
	drop(forwarded_payment_guard);
	let mut removed_payment_count = 0u64;
	for payment_id in payment_ids_to_remove {
		forwarded_payment_store.remove(&payment_id).await.map_err(|e| {
			log_error!(logger, "Failed to remove forwarded payment {:?}: {}", payment_id, e);
			e
		})?;
		removed_payment_count += 1;
	}

	Ok((aggregated_bucket_count, removed_payment_count))
}

/// Aggregates multiple channel-pair statistics buckets into cumulative totals.
///
/// The returned bucket spans from the earliest input bucket start through the latest input bucket
/// end, including any gaps between buckets.
///
/// Returns `None` if `buckets` is empty or contains statistics for different channel pairs.
pub fn aggregate_channel_pair_stats(
	buckets: &[ChannelPairForwardingStats],
) -> Option<ChannelPairForwardingStats> {
	let first = buckets.first()?;
	for bucket in &buckets[1..] {
		if bucket.prev_channel_id != first.prev_channel_id
			|| bucket.next_channel_id != first.next_channel_id
		{
			return None;
		}
	}

	let mut payment_count = 0u64;
	let mut total_inbound_amount_msat = 0u64;
	let mut total_outbound_amount_msat = 0u64;
	let mut total_fee_earned_msat = Some(0u64);
	let mut total_skimmed_fee_msat = 0u64;
	let mut onchain_claims_count = 0u64;
	let mut first_forwarded_at_timestamp = u64::MAX;
	let mut last_forwarded_at_timestamp = 0u64;
	let mut earliest_bucket_start = u64::MAX;
	let mut latest_bucket_end = 0u64;
	let mut prev_node_id = None;
	let mut next_node_id = None;
	for bucket in buckets {
		payment_count = payment_count.saturating_add(bucket.payment_count);
		total_inbound_amount_msat =
			total_inbound_amount_msat.saturating_add(bucket.total_inbound_amount_msat);
		total_outbound_amount_msat =
			total_outbound_amount_msat.saturating_add(bucket.total_outbound_amount_msat);
		total_fee_earned_msat = match (total_fee_earned_msat, bucket.total_fee_earned_msat) {
			(Some(total), Some(fee)) => Some(total.saturating_add(fee)),
			_ => None,
		};
		total_skimmed_fee_msat =
			total_skimmed_fee_msat.saturating_add(bucket.total_skimmed_fee_msat);
		onchain_claims_count = onchain_claims_count.saturating_add(bucket.onchain_claims_count);
		first_forwarded_at_timestamp =
			first_forwarded_at_timestamp.min(bucket.first_forwarded_at_timestamp);
		last_forwarded_at_timestamp =
			last_forwarded_at_timestamp.max(bucket.last_forwarded_at_timestamp);
		earliest_bucket_start = earliest_bucket_start.min(bucket.bucket_start_timestamp);
		latest_bucket_end = latest_bucket_end
			.max(bucket.bucket_start_timestamp.saturating_add(bucket.bucket_size_secs));
		if prev_node_id.is_none() {
			prev_node_id = bucket.prev_node_id;
		}
		if next_node_id.is_none() {
			next_node_id = bucket.next_node_id;
		}
	}
	let bucket_size_secs = latest_bucket_end.saturating_sub(earliest_bucket_start);
	let now =
		SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0)).as_secs();
	Some(ChannelPairForwardingStats {
		id: channel_pair_stats_id(
			&first.prev_channel_id,
			&first.next_channel_id,
			earliest_bucket_start,
			bucket_size_secs,
		),
		prev_channel_id: first.prev_channel_id,
		next_channel_id: first.next_channel_id,
		bucket_start_timestamp: earliest_bucket_start,
		bucket_size_secs,
		prev_node_id,
		next_node_id,
		payment_count,
		total_inbound_amount_msat,
		total_outbound_amount_msat,
		total_fee_earned_msat,
		total_skimmed_fee_msat,
		onchain_claims_count,
		avg_fee_msat: if payment_count > 0 {
			total_fee_earned_msat.map(|total| total / payment_count)
		} else {
			None
		},
		avg_inbound_amount_msat: if payment_count > 0 {
			total_inbound_amount_msat / payment_count
		} else {
			0
		},
		first_forwarded_at_timestamp,
		last_forwarded_at_timestamp,
		aggregated_at_timestamp: now,
	})
}

#[cfg(test)]
mod forwarding_stats_tests {
	use std::str::FromStr;

	use lightning::util::persist::{
		KVStore, KVSTORE_NAMESPACE_KEY_ALPHABET, KVSTORE_NAMESPACE_KEY_MAX_LEN,
	};

	use super::*;
	use crate::io::sqlite_store::SqliteStore;
	use crate::io::test_utils::{random_storage_path, InMemoryStore};
	use crate::types::{DynStore, DynStoreWrapper};

	type TestForwardedPaymentStore = DataStore<ForwardedPaymentDetails, Arc<Logger>>;
	type TestChannelPairStatsStore = DataStore<ChannelPairForwardingStats, Arc<Logger>>;

	fn test_stores() -> (TestForwardedPaymentStore, TestChannelPairStatsStore, Arc<Logger>) {
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(InMemoryStore::new()));
		let logger = Arc::new(Logger::new_log_facade());
		let forwarded_payment_store = DataStore::new(
			Vec::new(),
			"test_forwarded_payments".to_string(),
			String::new(),
			Arc::clone(&kv_store),
			Arc::clone(&logger),
		);
		let channel_pair_stats_store = DataStore::new(
			Vec::new(),
			"test_channel_pair_stats".to_string(),
			String::new(),
			kv_store,
			Arc::clone(&logger),
		);
		(forwarded_payment_store, channel_pair_stats_store, logger)
	}

	fn forwarded_payment(
		id: u8, forwarded_at_timestamp: u64, inbound_amount_msat: u64, outbound_amount_msat: u64,
		fee_msat: u64,
	) -> ForwardedPaymentDetails {
		ForwardedPaymentDetails {
			id: hex_utils::to_string(&[id; 32]),
			prev_channel_id: ChannelId([1; 32]),
			next_channel_id: ChannelId([2; 32]),
			prev_user_channel_id: None,
			next_user_channel_id: None,
			prev_node_id: None,
			next_node_id: None,
			inbound_amount_forwarded_msat: Some(inbound_amount_msat),
			total_fee_earned_msat: Some(fee_msat),
			skimmed_fee_msat: Some(0),
			claim_from_onchain_tx: false,
			outbound_amount_forwarded_msat: Some(outbound_amount_msat),
			forwarded_at_timestamp,
		}
	}

	fn channel_pair_stats(
		bucket_start_timestamp: u64, bucket_size_secs: u64, aggregated_at_timestamp: u64,
		payment_count: u64, total_inbound_amount_msat: u64, total_outbound_amount_msat: u64,
		total_fee_earned_msat: u64, first_forwarded_at_timestamp: u64,
		last_forwarded_at_timestamp: u64,
	) -> ChannelPairForwardingStats {
		let prev_channel_id = ChannelId([1; 32]);
		let next_channel_id = ChannelId([2; 32]);
		ChannelPairForwardingStats {
			id: channel_pair_stats_id(
				&prev_channel_id,
				&next_channel_id,
				bucket_start_timestamp,
				bucket_size_secs,
			),
			prev_channel_id,
			next_channel_id,
			bucket_start_timestamp,
			bucket_size_secs,
			prev_node_id: None,
			next_node_id: None,
			payment_count,
			total_inbound_amount_msat,
			total_outbound_amount_msat,
			total_fee_earned_msat: Some(total_fee_earned_msat),
			total_skimmed_fee_msat: 0,
			onchain_claims_count: 0,
			avg_fee_msat: Some(total_fee_earned_msat / payment_count),
			avg_inbound_amount_msat: total_inbound_amount_msat / payment_count,
			first_forwarded_at_timestamp,
			last_forwarded_at_timestamp,
			aggregated_at_timestamp,
		}
	}

	#[test]
	fn channel_pair_persistence_key_fits_kvstore_limit() {
		let id = channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 42, 60);
		let other_id = channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 43, 60);
		let other_size_id =
			channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 42, 120);

		let key = id.encode_to_key();
		assert_eq!(key.len(), 107);
		assert!(key.len() <= KVSTORE_NAMESPACE_KEY_MAX_LEN);
		assert!(key.chars().all(|c| KVSTORE_NAMESPACE_KEY_ALPHABET.contains(c)));
		assert_eq!(BASE64_URL_SAFE_NO_PAD.decode(&key).unwrap().len(), 80);
		assert_ne!(key, other_id.encode_to_key());
		assert_ne!(key, other_size_id.encode_to_key());
	}

	#[tokio::test]
	async fn aggregation_retains_current_and_previous_buckets() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let closed_bucket_payment = forwarded_payment(1, 899, 110, 100, 10);
		let partial_bucket_payment = forwarded_payment(2, 939, 220, 200, 20);
		forwarded_payment_store.insert(closed_bucket_payment.clone()).await.unwrap();
		forwarded_payment_store.insert(partial_bucket_payment.clone()).await.unwrap();

		// At timestamp 1,000, the current bucket starts at 960 and the previous bucket starts at
		// 900. Only payments older than the previous bucket are aggregated.
		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((1, 1))
		);

		assert!(forwarded_payment_store.get(&closed_bucket_payment.id).is_none());
		assert_eq!(
			forwarded_payment_store.get(&partial_bucket_payment.id),
			Some(partial_bucket_payment)
		);
		let bucket_id = channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 840, 60);
		let stats = channel_pair_stats_store.get(&bucket_id).unwrap();
		assert_eq!(stats.bucket_size_secs, 60);
		assert_eq!(stats.payment_count, 1);
		assert_eq!(stats.total_inbound_amount_msat, 110);
		assert_eq!(stats.total_outbound_amount_msat, 100);
		assert_eq!(stats.total_fee_earned_msat, Some(10));
		assert_eq!(stats.aggregated_at_timestamp, 1_000);
	}

	#[test]
	fn aggregation_schedule_aligns_to_bucket_closure() {
		assert_eq!(seconds_until_next_forwarding_aggregation(120, 60), 60);
		assert_eq!(seconds_until_next_forwarding_aggregation(121, 60), 59);
		assert_eq!(seconds_until_next_forwarding_aggregation(179, 60), 1);
	}

	#[tokio::test]
	async fn aggregation_preserves_unknown_fees() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let known_fee_payment = forwarded_payment(1, 850, 110, 100, 10);
		let mut unknown_fee_payment = forwarded_payment(2, 851, 200, 200, 0);
		unknown_fee_payment.total_fee_earned_msat = None;
		forwarded_payment_store.insert(known_fee_payment).await.unwrap();
		forwarded_payment_store.insert(unknown_fee_payment).await.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((1, 2))
		);

		let bucket_id = channel_pair_stats_id(&ChannelId([1; 32]), &ChannelId([2; 32]), 840, 60);
		let stats = channel_pair_stats_store.get(&bucket_id).unwrap();
		assert_eq!(stats.payment_count, 2);
		assert_eq!(stats.total_fee_earned_msat, None);
		assert_eq!(stats.avg_fee_msat, None);
	}

	#[test]
	fn channel_stats_update_preserves_unknown_fees() {
		let mut stats = ChannelForwardingStats {
			channel_id: ChannelId([1; 32]),
			counterparty_node_id: None,
			inbound_payments_forwarded: 1,
			outbound_payments_forwarded: 0,
			total_inbound_amount_msat: 110,
			total_outbound_amount_msat: 0,
			total_fee_earned_msat: Some(10),
			total_skimmed_fee_msat: 0,
			onchain_claims_count: 0,
			first_forwarded_at_timestamp: 850,
			last_forwarded_at_timestamp: 850,
		};

		assert!(stats.update(ChannelForwardingStatsUpdate {
			channel_id: stats.channel_id,
			counterparty_node_id: None,
			inbound_payments_increment: 1,
			outbound_payments_increment: 0,
			inbound_amount_increment_msat: 200,
			outbound_amount_increment_msat: 0,
			fee_earned_increment_msat: None,
			skimmed_fee_increment_msat: 0,
			onchain_claims_increment: 0,
			timestamp: 851,
		}));
		assert_eq!(stats.total_fee_earned_msat, None);
	}

	#[tokio::test]
	async fn aggregation_keeps_both_retained_bucket_boundaries() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let older_bucket_payment = forwarded_payment(1, 839, 110, 100, 10);
		let previous_bucket_payment = forwarded_payment(2, 840, 220, 200, 20);
		let current_bucket_payment = forwarded_payment(3, 900, 330, 300, 30);
		forwarded_payment_store.insert(older_bucket_payment.clone()).await.unwrap();
		forwarded_payment_store.insert(previous_bucket_payment.clone()).await.unwrap();
		forwarded_payment_store.insert(current_bucket_payment.clone()).await.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				60,
				900,
				&logger,
			)
			.await,
			Ok((1, 1))
		);

		assert!(forwarded_payment_store.get(&older_bucket_payment.id).is_none());
		assert_eq!(
			forwarded_payment_store.get(&previous_bucket_payment.id),
			Some(previous_bucket_payment)
		);
		assert_eq!(
			forwarded_payment_store.get(&current_bucket_payment.id),
			Some(current_bucket_payment)
		);
	}

	#[tokio::test]
	async fn zero_retention_cleans_up_after_the_current_bucket_closes() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let payment = forwarded_payment(1, 899, 110, 100, 10);
		forwarded_payment_store.insert(payment.clone()).await.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				0,
				899,
				&logger,
			)
			.await,
			Ok((0, 0))
		);
		assert_eq!(forwarded_payment_store.get(&payment.id), Some(payment.clone()));

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				0,
				900,
				&logger,
			)
			.await,
			Ok((1, 1))
		);
		assert!(forwarded_payment_store.get(&payment.id).is_none());
	}

	#[tokio::test]
	async fn aggregation_retry_only_cleans_up_committed_bucket() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let remaining_payment = forwarded_payment(2, 851, 220, 200, 20);
		let committed_stats = channel_pair_stats(840, 60, 950, 2, 330, 300, 30, 850, 851);
		channel_pair_stats_store.insert(committed_stats.clone()).await.unwrap();
		forwarded_payment_store.insert(remaining_payment.clone()).await.unwrap();

		// This represents a retry after the bucket write and one of two detail deletions
		// succeeded. The existing bucket is the commit marker, so its totals must not change.
		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				60,
				60,
				1_000,
				&logger,
			)
			.await,
			Ok((0, 1))
		);

		assert!(forwarded_payment_store.get(&remaining_payment.id).is_none());
		assert_eq!(channel_pair_stats_store.get(&committed_stats.id), Some(committed_stats));
	}

	#[test]
	fn channel_pair_update_refreshes_aggregation_timestamp() {
		let mut stats = channel_pair_stats(840, 60, 950, 2, 330, 300, 30, 850, 851);
		let update = channel_pair_stats(840, 60, 1_000, 1, 110, 100, 10, 852, 852);

		assert!(stats.update(update.to_update()));
		assert_eq!(stats.payment_count, 3);
		assert_eq!(stats.aggregated_at_timestamp, 1_000);
	}

	#[test]
	fn cumulative_stats_prefer_known_node_ids_and_cover_bucket_span() {
		let unknown_nodes = channel_pair_stats(840, 60, 950, 2, 330, 300, 30, 850, 851);
		let mut known_nodes = channel_pair_stats(900, 120, 1_000, 1, 110, 100, 10, 902, 902);
		let node_id = PublicKey::from_str(
			"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
		)
		.unwrap();
		known_nodes.prev_node_id = Some(node_id);
		known_nodes.next_node_id = Some(node_id);

		let cumulative = aggregate_channel_pair_stats(&[unknown_nodes, known_nodes]).unwrap();
		assert_eq!(cumulative.prev_node_id, Some(node_id));
		assert_eq!(cumulative.next_node_id, Some(node_id));
		assert_eq!(cumulative.bucket_start_timestamp, 840);
		assert_eq!(cumulative.bucket_size_secs, 180);
	}

	#[tokio::test]
	async fn retention_does_not_change_bucket_geometry() {
		let (forwarded_payment_store, channel_pair_stats_store, logger) = test_stores();
		let old_stats = channel_pair_stats(0, 3_600, 7_200, 1, 110, 100, 10, 100, 100);
		let payment = forwarded_payment(2, 4_000, 220, 200, 20);
		channel_pair_stats_store.insert(old_stats.clone()).await.unwrap();
		forwarded_payment_store.insert(payment.clone()).await.unwrap();

		assert_eq!(
			aggregate_expired_forwarded_payments_at(
				&forwarded_payment_store,
				&channel_pair_stats_store,
				3_600,
				7_200,
				15_000,
				&logger,
			)
			.await,
			Ok((1, 1))
		);

		let new_id =
			channel_pair_stats_id(&payment.prev_channel_id, &payment.next_channel_id, 3_600, 3_600);
		assert_eq!(channel_pair_stats_store.get(&old_stats.id), Some(old_stats));
		assert_eq!(channel_pair_stats_store.get(&new_id).unwrap().payment_count, 1);
		assert!(forwarded_payment_store.get(&payment.id).is_none());
	}

	#[tokio::test]
	async fn channel_pair_stats_persist_to_sqlite() {
		let mut storage_path = random_storage_path();
		storage_path.push("channel_pair_stats_persist_to_sqlite");
		let sqlite_store =
			SqliteStore::new(storage_path, Some("stats.sqlite".to_string()), None).unwrap();
		let kv_store: Arc<DynStore> = Arc::new(DynStoreWrapper(sqlite_store));
		let logger = Arc::new(Logger::new_log_facade());
		let namespace = "sqlite_channel_pair_stats";
		let stats_store = DataStore::new(
			Vec::new(),
			namespace.to_string(),
			String::new(),
			Arc::clone(&kv_store),
			logger,
		);
		let stats = channel_pair_stats(840, 60, 1_000, 2, 330, 300, 30, 850, 851);

		stats_store.insert(stats.clone()).await.unwrap();
		let keys = KVStore::list(&*kv_store, namespace, "").await.unwrap();
		assert_eq!(keys, vec![stats.id.encode_to_key()]);
		let bytes = KVStore::read(&*kv_store, namespace, "", &keys[0]).await.unwrap();
		assert_eq!(ChannelPairForwardingStats::read(&mut &bytes[..]).unwrap(), stats);
	}
}
