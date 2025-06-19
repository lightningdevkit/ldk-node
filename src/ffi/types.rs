// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

// Importing these items ensures they are accessible in the uniffi bindings
// without introducing unused import warnings in lib.rs.
//
// Make sure to add any re-exported items that need to be used in uniffi below.

pub use crate::config::{
	default_config, AnchorChannelsConfig, BackgroundSyncConfig, ElectrumSyncConfig,
	EsploraSyncConfig, MaxDustHTLCExposure,
};
pub use crate::graph::{ChannelInfo, ChannelUpdateInfo, NodeAnnouncementInfo, NodeInfo};
pub use crate::liquidity::{LSPS1OrderStatus, LSPS2ServiceConfig, OnchainPaymentInfo, PaymentInfo};
pub use crate::logger::{LogLevel, LogRecord, LogWriter};
pub use crate::payment::store::{
	ConfirmationStatus, LSPFeeLimits, PaymentDirection, PaymentKind, PaymentStatus,
};
pub use crate::payment::{MaxTotalRoutingFeeLimit, QrPaymentResult, SendingParameters};

pub use lightning::chain::channelmonitor::BalanceSource;
pub use lightning::events::{ClosureReason, PaymentFailureReason};
pub use lightning::ln::types::ChannelId;
pub use lightning::offers::offer::OfferId;
pub use lightning::routing::gossip::{NodeAlias, NodeId, RoutingFees};
pub use lightning::util::string::UntrustedString;

pub use lightning_types::payment::{PaymentHash, PaymentPreimage, PaymentSecret};

pub use lightning_invoice::{Description, SignedRawBolt11Invoice};

pub use lightning_liquidity::lsps1::msgs::ChannelInfo as ChannelOrderInfo;
pub use lightning_liquidity::lsps1::msgs::{OrderId, OrderParameters, PaymentState};

pub use bitcoin::{Address, BlockHash, FeeRate, Network, OutPoint, Txid};

pub use bip39::Mnemonic;

pub use vss_client::headers::{VssHeaderProvider, VssHeaderProviderError};

pub type DateTime = chrono::DateTime<chrono::Utc>;

use crate::UniffiCustomTypeConverter;

use crate::builder::sanitize_alias;
use crate::error::Error;
use crate::hex_utils;
use crate::{SocketAddress, UserChannelId};

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::invoice::Bolt12Invoice as LdkBolt12Invoice;
use lightning::offers::offer::{Amount as LdkAmount, Offer as LdkOffer};
use lightning::offers::refund::Refund as LdkRefund;
use lightning::util::ser::Writeable;
use lightning_invoice::{Bolt11Invoice as LdkBolt11Invoice, Bolt11InvoiceDescriptionRef};

use std::convert::TryInto;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

impl UniffiCustomTypeConverter for PublicKey {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(key) = PublicKey::from_str(&val) {
			return Ok(key);
		}

		Err(Error::InvalidPublicKey.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for NodeId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(key) = NodeId::from_str(&val) {
			return Ok(key);
		}

		Err(Error::InvalidNodeId.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Address {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(addr) = Address::from_str(&val) {
			return Ok(addr.assume_checked());
		}

		Err(Error::InvalidAddress.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferAmount {
	Bitcoin { amount_msats: u64 },
	Currency { iso4217_code: String, amount: u64 },
}

impl From<LdkAmount> for OfferAmount {
	fn from(ldk_amount: LdkAmount) -> Self {
		match ldk_amount {
			LdkAmount::Bitcoin { amount_msats } => OfferAmount::Bitcoin { amount_msats },
			LdkAmount::Currency { iso4217_code, amount } => OfferAmount::Currency {
				iso4217_code: iso4217_code.iter().map(|&b| b as char).collect(),
				amount,
			},
		}
	}
}

/// An `Offer` is a potentially long-lived proposal for payment of a good or service.
///
/// An offer is a precursor to an [`InvoiceRequest`]. A merchant publishes an offer from which a
/// customer may request an [`Bolt12Invoice`] for a specific quantity and using an amount sufficient
/// to cover that quantity (i.e., at least `quantity * amount`). See [`Offer::amount`].
///
/// Offers may be denominated in currency other than bitcoin but are ultimately paid using the
/// latter.
///
/// Through the use of [`BlindedMessagePath`]s, offers provide recipient privacy.
///
/// [`InvoiceRequest`]: lightning::offers::invoice_request::InvoiceRequest
/// [`Bolt12Invoice`]: lightning::offers::invoice::Bolt12Invoice
/// [`Offer`]: lightning::offers::Offer:amount
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
	pub(crate) inner: LdkOffer,
}

impl Offer {
	pub fn from_str(offer_str: &str) -> Result<Self, Error> {
		offer_str.parse()
	}

	/// Returns the id of the offer.
	pub fn id(&self) -> OfferId {
		OfferId(self.inner.id().0)
	}

	/// Whether the offer has expired.
	pub fn is_expired(&self) -> bool {
		self.inner.is_expired()
	}

	/// A complete description of the purpose of the payment.
	///
	/// Intended to be displayed to the user but with the caveat that it has not been verified in any way.
	pub fn description(&self) -> Option<String> {
		self.inner.description().map(|printable| printable.to_string())
	}

	/// The issuer of the offer, possibly beginning with `user@domain` or `domain`.
	///
	/// Intended to be displayed to the user but with the caveat that it has not been verified in any way.
	pub fn issuer(&self) -> Option<String> {
		self.inner.issuer().map(|printable| printable.to_string())
	}

	/// The minimum amount required for a successful payment of a single item.
	pub fn amount(&self) -> Option<OfferAmount> {
		self.inner.amount().map(|amount| amount.into())
	}

	/// Returns whether the given quantity is valid for the offer.
	pub fn is_valid_quantity(&self, quantity: u64) -> bool {
		self.inner.is_valid_quantity(quantity)
	}

	/// Returns whether a quantity is expected in an [`InvoiceRequest`] for the offer.
	///
	/// [`InvoiceRequest`]: lightning::offers::invoice_request::InvoiceRequest
	pub fn expects_quantity(&self) -> bool {
		self.inner.expects_quantity()
	}

	/// Returns whether the given chain is supported by the offer.
	pub fn supports_chain(&self, chain: Network) -> bool {
		self.inner.supports_chain(chain.chain_hash())
	}

	/// The chains that may be used when paying a requested invoice (e.g., bitcoin mainnet).
	///
	/// Payments must be denominated in units of the minimal lightning-payable unit (e.g., msats)
	/// for the selected chain.
	pub fn chains(&self) -> Vec<Network> {
		self.inner.chains().into_iter().filter_map(Network::from_chain_hash).collect()
	}

	/// Opaque bytes set by the originator.
	///
	/// Useful for authentication and validating fields since it is reflected in `invoice_request`
	/// messages along with all the other fields from the `offer`.
	pub fn metadata(&self) -> Option<Vec<u8>> {
		self.inner.metadata().cloned()
	}

	/// Seconds since the Unix epoch when an invoice should no longer be requested.
	///
	/// If `None`, the offer does not expire.
	pub fn absolute_expiry_seconds(&self) -> Option<u64> {
		self.inner.absolute_expiry().map(|duration| duration.as_secs())
	}

	/// The public key corresponding to the key used by the recipient to sign invoices.
	/// - If [`Offer::paths`] is empty, MUST be `Some` and contain the recipient's node id for
	///   sending an [`InvoiceRequest`].
	/// - If [`Offer::paths`] is not empty, MAY be `Some` and contain a transient id.
	/// - If `None`, the signing pubkey will be the final blinded node id from the
	///   [`BlindedMessagePath`] in [`Offer::paths`] used to send the [`InvoiceRequest`].
	///
	/// See also [`Bolt12Invoice::signing_pubkey`].
	///
	/// [`InvoiceRequest`]: lightning::offers::invoice_request::InvoiceRequest
	/// [`Bolt12Invoice::signing_pubkey`]: lightning::offers::invoice::Bolt12Invoice::signing_pubkey
	pub fn issuer_signing_pubkey(&self) -> Option<PublicKey> {
		self.inner.issuer_signing_pubkey()
	}
}

impl std::str::FromStr for Offer {
	type Err = Error;

	fn from_str(offer_str: &str) -> Result<Self, Self::Err> {
		offer_str
			.parse::<LdkOffer>()
			.map(|offer| Offer { inner: offer })
			.map_err(|_| Error::InvalidOffer)
	}
}

impl From<LdkOffer> for Offer {
	fn from(offer: LdkOffer) -> Self {
		Offer { inner: offer }
	}
}

impl Deref for Offer {
	type Target = LdkOffer;
	fn deref(&self) -> &Self::Target {
		&self.inner
	}
}

impl AsRef<LdkOffer> for Offer {
	fn as_ref(&self) -> &LdkOffer {
		self.deref()
	}
}

impl std::fmt::Display for Offer {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.inner)
	}
}

/// A `Refund` is a request to send an [`Bolt12Invoice`] without a preceding [`Offer`].
///
/// Typically, after an invoice is paid, the recipient may publish a refund allowing the sender to
/// recoup their funds. A refund may be used more generally as an "offer for money", such as with a
/// bitcoin ATM.
///
/// [`Bolt12Invoice`]: lightning::offers::invoice::Bolt12Invoice
/// [`Offer`]: lightning::offers::offer::Offer
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refund {
	pub(crate) inner: LdkRefund,
}

impl Refund {
	pub fn from_str(refund_str: &str) -> Result<Self, Error> {
		refund_str.parse()
	}

	/// A complete description of the purpose of the refund.
	///
	/// Intended to be displayed to the user but with the caveat that it has not been verified in any way.
	pub fn description(&self) -> String {
		self.inner.description().to_string()
	}

	/// Seconds since the Unix epoch when an invoice should no longer be sent.
	///
	/// If `None`, the refund does not expire.
	pub fn absolute_expiry_seconds(&self) -> Option<u64> {
		self.inner.absolute_expiry().map(|duration| duration.as_secs())
	}

	/// Whether the refund has expired.
	pub fn is_expired(&self) -> bool {
		self.inner.is_expired()
	}

	/// The issuer of the refund, possibly beginning with `user@domain` or `domain`.
	///
	/// Intended to be displayed to the user but with the caveat that it has not been verified in any way.
	pub fn issuer(&self) -> Option<String> {
		self.inner.issuer().map(|printable| printable.to_string())
	}

	/// An unpredictable series of bytes, typically containing information about the derivation of
	/// [`payer_signing_pubkey`].
	///
	/// [`payer_signing_pubkey`]: Self::payer_signing_pubkey
	pub fn payer_metadata(&self) -> Vec<u8> {
		self.inner.payer_metadata().to_vec()
	}

	/// A chain that the refund is valid for.
	pub fn chain(&self) -> Option<Network> {
		Network::try_from(self.inner.chain()).ok()
	}

	/// The amount to refund in msats (i.e., the minimum lightning-payable unit for [`chain`]).
	///
	/// [`chain`]: Self::chain
	pub fn amount_msats(&self) -> u64 {
		self.inner.amount_msats()
	}

	/// The quantity of an item that refund is for.
	pub fn quantity(&self) -> Option<u64> {
		self.inner.quantity()
	}

	/// A public node id to send to in the case where there are no [`paths`].
	///
	/// Otherwise, a possibly transient pubkey.
	///
	/// [`paths`]: lightning::offers::refund::Refund::paths
	pub fn payer_signing_pubkey(&self) -> PublicKey {
		self.inner.payer_signing_pubkey()
	}

	/// Payer provided note to include in the invoice.
	pub fn payer_note(&self) -> Option<String> {
		self.inner.payer_note().map(|printable| printable.to_string())
	}
}

impl std::str::FromStr for Refund {
	type Err = Error;

	fn from_str(refund_str: &str) -> Result<Self, Self::Err> {
		refund_str
			.parse::<LdkRefund>()
			.map(|refund| Refund { inner: refund })
			.map_err(|_| Error::InvalidRefund)
	}
}

impl From<LdkRefund> for Refund {
	fn from(refund: LdkRefund) -> Self {
		Refund { inner: refund }
	}
}

impl Deref for Refund {
	type Target = LdkRefund;
	fn deref(&self) -> &Self::Target {
		&self.inner
	}
}

impl AsRef<LdkRefund> for Refund {
	fn as_ref(&self) -> &LdkRefund {
		self.deref()
	}
}

impl std::fmt::Display for Refund {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.inner)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bolt12Invoice {
	pub(crate) inner: LdkBolt12Invoice,
}

impl Bolt12Invoice {
	pub fn from_str(invoice_str: &str) -> Result<Self, Error> {
		invoice_str.parse()
	}

	/// SHA256 hash of the payment preimage that will be given in return for paying the invoice.
	pub fn payment_hash(&self) -> PaymentHash {
		PaymentHash(self.inner.payment_hash().0)
	}

	/// The minimum amount required for a successful payment of the invoice.
	pub fn amount_msats(&self) -> u64 {
		self.inner.amount_msats()
	}

	/// The minimum amount required for a successful payment of a single item.
	///
	/// From [`Offer::amount`]; `None` if the invoice was created in response to a [`Refund`] or if
	/// the [`Offer`] did not set it.
	///
	/// [`Offer`]: lightning::offers::offer::Offer
	/// [`Offer::amount`]: lightning::offers::offer::Offer::amount
	/// [`Refund`]: lightning::offers::refund::Refund
	pub fn amount(&self) -> Option<OfferAmount> {
		self.inner.amount().map(|amount| amount.into())
	}

	/// A typically transient public key corresponding to the key used to sign the invoice.
	///
	/// If the invoices was created in response to an [`Offer`], then this will be:
	/// - [`Offer::issuer_signing_pubkey`] if it's `Some`, otherwise
	/// - the final blinded node id from a [`BlindedMessagePath`] in [`Offer::paths`] if `None`.
	///
	/// If the invoice was created in response to a [`Refund`], then it is a valid pubkey chosen by
	/// the recipient.
	///
	/// [`Offer`]: lightning::offers::offer::Offer
	/// [`Offer::issuer_signing_pubkey`]: lightning::offers::offer::Offer::issuer_signing_pubkey
	/// [`Offer::paths`]: lightning::offers::offer::Offer::paths
	/// [`Refund`]: lightning::offers::refund::Refund
	pub fn signing_pubkey(&self) -> PublicKey {
		self.inner.signing_pubkey()
	}

	/// Duration since the Unix epoch when the invoice was created.
	pub fn created_at(&self) -> u64 {
		self.inner.created_at().as_secs()
	}

	/// Seconds since the Unix epoch when an invoice should no longer be requested.
	///
	/// From [`Offer::absolute_expiry`] or [`Refund::absolute_expiry`].
	///
	/// [`Offer::absolute_expiry`]: lightning::offers::offer::Offer::absolute_expiry
	pub fn absolute_expiry_seconds(&self) -> Option<u64> {
		self.inner.absolute_expiry().map(|duration| duration.as_secs())
	}

	/// When the invoice has expired and therefore should no longer be paid.
	pub fn relative_expiry(&self) -> u64 {
		self.inner.relative_expiry().as_secs()
	}

	/// Whether the invoice has expired.
	pub fn is_expired(&self) -> bool {
		self.inner.is_expired()
	}

	/// A complete description of the purpose of the originating offer or refund.
	///
	/// From [`Offer::description`] or [`Refund::description`].
	///
	/// [`Offer::description`]: lightning::offers::offer::Offer::description
	/// [`Refund::description`]: lightning::offers::refund::Refund::description
	pub fn description(&self) -> Option<String> {
		self.inner.description().map(|printable| printable.to_string())
	}

	/// The issuer of the offer or refund.
	///
	/// From [`Offer::issuer`] or [`Refund::issuer`].
	///
	/// [`Offer::issuer`]: lightning::offers::offer::Offer::issuer
	/// [`Refund::issuer`]: lightning::offers::refund::Refund::issuer
	pub fn issuer(&self) -> Option<String> {
		self.inner.issuer().map(|printable| printable.to_string())
	}

	/// A payer-provided note reflected back in the invoice.
	///
	/// From [`InvoiceRequest::payer_note`] or [`Refund::payer_note`].
	///
	/// [`Refund::payer_note`]: lightning::offers::refund::Refund::payer_note
	pub fn payer_note(&self) -> Option<String> {
		self.inner.payer_note().map(|note| note.to_string())
	}

	/// Opaque bytes set by the originating [`Offer`].
	///
	/// From [`Offer::metadata`]; `None` if the invoice was created in response to a [`Refund`] or
	/// if the [`Offer`] did not set it.
	///
	/// [`Offer`]: lightning::offers::offer::Offer
	/// [`Offer::metadata`]: lightning::offers::offer::Offer::metadata
	/// [`Refund`]: lightning::offers::refund::Refund
	pub fn metadata(&self) -> Option<Vec<u8>> {
		self.inner.metadata().cloned()
	}

	/// The quantity of items requested or refunded for.
	///
	/// From [`InvoiceRequest::quantity`] or [`Refund::quantity`].
	///
	/// [`Refund::quantity`]: lightning::offers::refund::Refund::quantity
	pub fn quantity(&self) -> Option<u64> {
		self.inner.quantity()
	}

	/// Hash that was used for signing the invoice.
	pub fn signable_hash(&self) -> Vec<u8> {
		self.inner.signable_hash().to_vec()
	}

	/// A possibly transient pubkey used to sign the invoice request or to send an invoice for a
	/// refund in case there are no [`message_paths`].
	///
	/// [`message_paths`]: lightning::offers::invoice::Bolt12Invoice
	pub fn payer_signing_pubkey(&self) -> PublicKey {
		self.inner.payer_signing_pubkey()
	}

	/// The public key used by the recipient to sign invoices.
	///
	/// From [`Offer::issuer_signing_pubkey`] and may be `None`; also `None` if the invoice was
	/// created in response to a [`Refund`].
	///
	/// [`Offer::issuer_signing_pubkey`]: lightning::offers::offer::Offer::issuer_signing_pubkey
	/// [`Refund`]: lightning::offers::refund::Refund
	pub fn issuer_signing_pubkey(&self) -> Option<PublicKey> {
		self.inner.issuer_signing_pubkey()
	}

	/// The chain that must be used when paying the invoice; selected from [`offer_chains`] if the
	/// invoice originated from an offer.
	///
	/// From [`InvoiceRequest::chain`] or [`Refund::chain`].
	///
	/// [`offer_chains`]: lightning::offers::invoice::Bolt12Invoice::offer_chains
	/// [`InvoiceRequest::chain`]: lightning::offers::invoice_request::InvoiceRequest::chain
	/// [`Refund::chain`]: lightning::offers::refund::Refund::chain
	pub fn chain(&self) -> Vec<u8> {
		self.inner.chain().to_bytes().to_vec()
	}

	/// The chains that may be used when paying a requested invoice.
	///
	/// From [`Offer::chains`]; `None` if the invoice was created in response to a [`Refund`].
	///
	/// [`Offer::chains`]: lightning::offers::offer::Offer::chains
	/// [`Refund`]: lightning::offers::refund::Refund
	pub fn offer_chains(&self) -> Option<Vec<Vec<u8>>> {
		self.inner
			.offer_chains()
			.map(|chains| chains.iter().map(|chain| chain.to_bytes().to_vec()).collect())
	}

	/// Fallback addresses for paying the invoice on-chain, in order of most-preferred to
	/// least-preferred.
	pub fn fallback_addresses(&self) -> Vec<Address> {
		self.inner.fallbacks()
	}

	/// Writes `self` out to a `Vec<u8>`.
	pub fn encode(&self) -> Vec<u8> {
		self.inner.encode()
	}
}

impl std::str::FromStr for Bolt12Invoice {
	type Err = Error;

	fn from_str(invoice_str: &str) -> Result<Self, Self::Err> {
		if let Some(bytes_vec) = hex_utils::to_vec(invoice_str) {
			if let Ok(invoice) = LdkBolt12Invoice::try_from(bytes_vec) {
				return Ok(Bolt12Invoice { inner: invoice });
			}
		}
		Err(Error::InvalidInvoice)
	}
}

impl From<LdkBolt12Invoice> for Bolt12Invoice {
	fn from(invoice: LdkBolt12Invoice) -> Self {
		Bolt12Invoice { inner: invoice }
	}
}

impl Deref for Bolt12Invoice {
	type Target = LdkBolt12Invoice;
	fn deref(&self) -> &Self::Target {
		&self.inner
	}
}

impl AsRef<LdkBolt12Invoice> for Bolt12Invoice {
	fn as_ref(&self) -> &LdkBolt12Invoice {
		self.deref()
	}
}

impl UniffiCustomTypeConverter for OfferId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(bytes_vec) = hex_utils::to_vec(&val) {
			let bytes_res = bytes_vec.try_into();
			if let Ok(bytes) = bytes_res {
				return Ok(OfferId(bytes));
			}
		}
		Err(Error::InvalidOfferId.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

impl UniffiCustomTypeConverter for PaymentId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(bytes_vec) = hex_utils::to_vec(&val) {
			let bytes_res = bytes_vec.try_into();
			if let Ok(bytes) = bytes_res {
				return Ok(PaymentId(bytes));
			}
		}
		Err(Error::InvalidPaymentId.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

impl UniffiCustomTypeConverter for PaymentHash {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(hash) = Sha256::from_str(&val) {
			Ok(PaymentHash(hash.to_byte_array()))
		} else {
			Err(Error::InvalidPaymentHash.into())
		}
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		Sha256::from_slice(&obj.0).unwrap().to_string()
	}
}

impl UniffiCustomTypeConverter for PaymentPreimage {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(bytes_vec) = hex_utils::to_vec(&val) {
			let bytes_res = bytes_vec.try_into();
			if let Ok(bytes) = bytes_res {
				return Ok(PaymentPreimage(bytes));
			}
		}
		Err(Error::InvalidPaymentPreimage.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

impl UniffiCustomTypeConverter for PaymentSecret {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(bytes_vec) = hex_utils::to_vec(&val) {
			let bytes_res = bytes_vec.try_into();
			if let Ok(bytes) = bytes_res {
				return Ok(PaymentSecret(bytes));
			}
		}
		Err(Error::InvalidPaymentSecret.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

impl UniffiCustomTypeConverter for ChannelId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(hex_vec) = hex_utils::to_vec(&val) {
			if hex_vec.len() == 32 {
				let mut channel_id = [0u8; 32];
				channel_id.copy_from_slice(&hex_vec[..]);
				return Ok(Self(channel_id));
			}
		}
		Err(Error::InvalidChannelId.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.0)
	}
}

impl UniffiCustomTypeConverter for UserChannelId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(UserChannelId(u128::from_str(&val).map_err(|_| Error::InvalidChannelId)?))
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.0.to_string()
	}
}

impl UniffiCustomTypeConverter for Txid {
	type Builtin = String;
	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(Txid::from_str(&val)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for BlockHash {
	type Builtin = String;
	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(BlockHash::from_str(&val)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Mnemonic {
	type Builtin = String;
	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(Mnemonic::from_str(&val).map_err(|_| Error::InvalidSecretKey)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for SocketAddress {
	type Builtin = String;
	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(SocketAddress::from_str(&val).map_err(|_| Error::InvalidSocketAddress)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for UntrustedString {
	type Builtin = String;
	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(UntrustedString(val))
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for NodeAlias {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(sanitize_alias(&val).map_err(|_| Error::InvalidNodeAlias)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

/// Represents the description of an invoice which has to be either a directly included string or
/// a hash of a description provided out of band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bolt11InvoiceDescription {
	/// Contains a full description.
	Direct {
		/// Description of what the invoice is for
		description: String,
	},
	/// Contains a hash.
	Hash {
		/// Hash of the description of what the invoice is for
		hash: String,
	},
}

impl TryFrom<&Bolt11InvoiceDescription> for lightning_invoice::Bolt11InvoiceDescription {
	type Error = Error;

	fn try_from(value: &Bolt11InvoiceDescription) -> Result<Self, Self::Error> {
		match value {
			Bolt11InvoiceDescription::Direct { description } => {
				Description::new(description.clone())
					.map(lightning_invoice::Bolt11InvoiceDescription::Direct)
					.map_err(|_| Error::InvoiceCreationFailed)
			},
			Bolt11InvoiceDescription::Hash { hash } => Sha256::from_str(&hash)
				.map(lightning_invoice::Sha256)
				.map(lightning_invoice::Bolt11InvoiceDescription::Hash)
				.map_err(|_| Error::InvoiceCreationFailed),
		}
	}
}

impl From<lightning_invoice::Bolt11InvoiceDescription> for Bolt11InvoiceDescription {
	fn from(value: lightning_invoice::Bolt11InvoiceDescription) -> Self {
		match value {
			lightning_invoice::Bolt11InvoiceDescription::Direct(description) => {
				Bolt11InvoiceDescription::Direct { description: description.to_string() }
			},
			lightning_invoice::Bolt11InvoiceDescription::Hash(hash) => {
				Bolt11InvoiceDescription::Hash { hash: hex_utils::to_string(hash.0.as_ref()) }
			},
		}
	}
}

impl<'a> From<Bolt11InvoiceDescriptionRef<'a>> for Bolt11InvoiceDescription {
	fn from(value: Bolt11InvoiceDescriptionRef<'a>) -> Self {
		match value {
			lightning_invoice::Bolt11InvoiceDescriptionRef::Direct(description) => {
				Bolt11InvoiceDescription::Direct { description: description.to_string() }
			},
			lightning_invoice::Bolt11InvoiceDescriptionRef::Hash(hash) => {
				Bolt11InvoiceDescription::Hash { hash: hex_utils::to_string(hash.0.as_ref()) }
			},
		}
	}
}

/// Enum representing the crypto currencies (or networks) supported by this library
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Currency {
	/// Bitcoin mainnet
	Bitcoin,

	/// Bitcoin testnet
	BitcoinTestnet,

	/// Bitcoin regtest
	Regtest,

	/// Bitcoin simnet
	Simnet,

	/// Bitcoin signet
	Signet,
}

impl From<lightning_invoice::Currency> for Currency {
	fn from(currency: lightning_invoice::Currency) -> Self {
		match currency {
			lightning_invoice::Currency::Bitcoin => Currency::Bitcoin,
			lightning_invoice::Currency::BitcoinTestnet => Currency::BitcoinTestnet,
			lightning_invoice::Currency::Regtest => Currency::Regtest,
			lightning_invoice::Currency::Simnet => Currency::Simnet,
			lightning_invoice::Currency::Signet => Currency::Signet,
		}
	}
}

/// A channel descriptor for a hop along a payment path.
///
/// While this generally comes from BOLT 11's `r` field, this struct includes more fields than are
/// available in BOLT 11.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteHintHop {
	/// The node_id of the non-target end of the route
	pub src_node_id: PublicKey,
	/// The short_channel_id of this channel
	pub short_channel_id: u64,
	/// The fees which must be paid to use this channel
	pub fees: RoutingFees,
	/// The difference in CLTV values between this node and the next node.
	pub cltv_expiry_delta: u16,
	/// The minimum value, in msat, which must be relayed to the next hop.
	pub htlc_minimum_msat: Option<u64>,
	/// The maximum value in msat available for routing with a single HTLC.
	pub htlc_maximum_msat: Option<u64>,
}

impl From<lightning::routing::router::RouteHintHop> for RouteHintHop {
	fn from(hop: lightning::routing::router::RouteHintHop) -> Self {
		Self {
			src_node_id: hop.src_node_id,
			short_channel_id: hop.short_channel_id,
			cltv_expiry_delta: hop.cltv_expiry_delta,
			htlc_minimum_msat: hop.htlc_minimum_msat,
			htlc_maximum_msat: hop.htlc_maximum_msat,
			fees: hop.fees,
		}
	}
}

/// Represents a syntactically and semantically correct lightning BOLT11 invoice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bolt11Invoice {
	pub(crate) inner: LdkBolt11Invoice,
}

impl Bolt11Invoice {
	pub fn from_str(invoice_str: &str) -> Result<Self, Error> {
		invoice_str.parse()
	}

	/// The hash of the [`RawBolt11Invoice`] that was signed.
	///
	/// [`RawBolt11Invoice`]: lightning_invoice::RawBolt11Invoice
	pub fn signable_hash(&self) -> Vec<u8> {
		self.inner.signable_hash().to_vec()
	}

	/// Returns the hash to which we will receive the preimage on completion of the payment
	pub fn payment_hash(&self) -> PaymentHash {
		PaymentHash(self.inner.payment_hash().to_byte_array())
	}

	/// Get the payment secret if one was included in the invoice
	pub fn payment_secret(&self) -> PaymentSecret {
		PaymentSecret(self.inner.payment_secret().0)
	}

	/// Returns the amount if specified in the invoice as millisatoshis.
	pub fn amount_milli_satoshis(&self) -> Option<u64> {
		self.inner.amount_milli_satoshis()
	}

	/// Returns the invoice's expiry time (in seconds), if present, otherwise [`DEFAULT_EXPIRY_TIME`].
	///
	/// [`DEFAULT_EXPIRY_TIME`]: lightning_invoice::DEFAULT_EXPIRY_TIME
	pub fn expiry_time_seconds(&self) -> u64 {
		self.inner.expiry_time().as_secs()
	}

	/// Returns the `Bolt11Invoice`'s timestamp as seconds since the Unix epoch
	pub fn seconds_since_epoch(&self) -> u64 {
		self.inner.duration_since_epoch().as_secs()
	}

	/// Returns the seconds remaining until the invoice expires.
	pub fn seconds_until_expiry(&self) -> u64 {
		self.inner.duration_until_expiry().as_secs()
	}

	/// Returns whether the invoice has expired.
	pub fn is_expired(&self) -> bool {
		self.inner.is_expired()
	}

	/// Returns whether the expiry time would pass at the given point in time.
	/// `at_time_seconds` is the timestamp as seconds since the Unix epoch.
	pub fn would_expire(&self, at_time_seconds: u64) -> bool {
		self.inner.would_expire(Duration::from_secs(at_time_seconds))
	}

	/// Return the description or a hash of it for longer ones
	pub fn invoice_description(&self) -> Bolt11InvoiceDescription {
		self.inner.description().into()
	}

	/// Returns the invoice's `min_final_cltv_expiry_delta` time, if present, otherwise
	/// [`DEFAULT_MIN_FINAL_CLTV_EXPIRY_DELTA`].
	///
	/// [`DEFAULT_MIN_FINAL_CLTV_EXPIRY_DELTA`]: lightning_invoice::DEFAULT_MIN_FINAL_CLTV_EXPIRY_DELTA
	pub fn min_final_cltv_expiry_delta(&self) -> u64 {
		self.inner.min_final_cltv_expiry_delta()
	}

	/// Returns the network for which the invoice was issued
	pub fn network(&self) -> Network {
		self.inner.network()
	}

	/// Returns the currency for which the invoice was issued
	pub fn currency(&self) -> Currency {
		self.inner.currency().into()
	}

	/// Returns a list of all fallback addresses as [`Address`]es
	pub fn fallback_addresses(&self) -> Vec<Address> {
		self.inner.fallback_addresses()
	}

	/// Returns a list of all routes included in the invoice as the underlying hints
	pub fn route_hints(&self) -> Vec<Vec<RouteHintHop>> {
		self.inner
			.route_hints()
			.iter()
			.map(|route| route.0.iter().map(|hop| RouteHintHop::from(hop.clone())).collect())
			.collect()
	}

	/// Recover the payee's public key (only to be used if none was included in the invoice)
	pub fn recover_payee_pub_key(&self) -> PublicKey {
		self.inner.recover_payee_pub_key()
	}
}

impl std::str::FromStr for Bolt11Invoice {
	type Err = Error;

	fn from_str(invoice_str: &str) -> Result<Self, Self::Err> {
		match invoice_str.parse::<SignedRawBolt11Invoice>() {
			Ok(signed) => match LdkBolt11Invoice::from_signed(signed) {
				Ok(invoice) => Ok(Bolt11Invoice { inner: invoice }),
				Err(_) => Err(Error::InvalidInvoice),
			},
			Err(_) => Err(Error::InvalidInvoice),
		}
	}
}

impl From<LdkBolt11Invoice> for Bolt11Invoice {
	fn from(invoice: LdkBolt11Invoice) -> Self {
		Bolt11Invoice { inner: invoice }
	}
}

impl Deref for Bolt11Invoice {
	type Target = LdkBolt11Invoice;
	fn deref(&self) -> &Self::Target {
		&self.inner
	}
}

impl AsRef<LdkBolt11Invoice> for Bolt11Invoice {
	fn as_ref(&self) -> &LdkBolt11Invoice {
		self.deref()
	}
}

impl std::fmt::Display for Bolt11Invoice {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.inner)
	}
}

/// A Lightning payment using BOLT 11.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bolt11PaymentInfo {
	/// Indicates the current state of the payment.
	pub state: PaymentState,
	/// The datetime when the payment option expires.
	pub expires_at: chrono::DateTime<chrono::Utc>,
	/// The total fee the LSP will charge to open this channel in satoshi.
	pub fee_total_sat: u64,
	/// The amount the client needs to pay to have the requested channel openend.
	pub order_total_sat: u64,
	/// A BOLT11 invoice the client can pay to have to channel opened.
	pub invoice: Arc<Bolt11Invoice>,
}

impl From<lightning_liquidity::lsps1::msgs::Bolt11PaymentInfo> for Bolt11PaymentInfo {
	fn from(info: lightning_liquidity::lsps1::msgs::Bolt11PaymentInfo) -> Self {
		Self {
			state: info.state,
			expires_at: info.expires_at,
			fee_total_sat: info.fee_total_sat,
			order_total_sat: info.order_total_sat,
			invoice: Arc::new(info.invoice.into()),
		}
	}
}

impl UniffiCustomTypeConverter for OrderId {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(Self(val))
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.0
	}
}

impl UniffiCustomTypeConverter for DateTime {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Ok(DateTime::from_str(&val).map_err(|_| Error::InvalidDateTime)?)
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_rfc3339()
	}
}

#[cfg(test)]
mod tests {
	use std::{
		num::NonZeroU64,
		time::{SystemTime, UNIX_EPOCH},
	};

	use super::*;
	use lightning::offers::{
		offer::{OfferBuilder, Quantity},
		refund::RefundBuilder,
	};

	fn create_test_bolt11_invoice() -> (LdkBolt11Invoice, Bolt11Invoice) {
		let invoice_string = "lnbc1pn8g249pp5f6ytj32ty90jhvw69enf30hwfgdhyymjewywcmfjevflg6s4z86qdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdfjjzh2j9f7ye5qqqqryqqqqthqqpysp5mm832athgcal3m7h35sc29j63lmgzvwc5smfjh2es65elc2ns7dq9qrsgqu2xcje2gsnjp0wn97aknyd3h58an7sjj6nhcrm40846jxphv47958c6th76whmec8ttr2wmg6sxwchvxmsc00kqrzqcga6lvsf9jtqgqy5yexa";
		let ldk_invoice: LdkBolt11Invoice = invoice_string.parse().unwrap();
		let wrapped_invoice = Bolt11Invoice::from(ldk_invoice.clone());
		(ldk_invoice, wrapped_invoice)
	}

	fn create_test_offer() -> (LdkOffer, Offer) {
		let pubkey = bitcoin::secp256k1::PublicKey::from_str(
			"02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
		)
		.unwrap();

		let expiry =
			(SystemTime::now() + Duration::from_secs(3600)).duration_since(UNIX_EPOCH).unwrap();

		let quantity = NonZeroU64::new(10_000).unwrap();

		let builder = OfferBuilder::new(pubkey)
			.description("Test offer description".to_string())
			.amount_msats(100_000)
			.issuer("Offer issuer".to_string())
			.absolute_expiry(expiry)
			.chain(Network::Bitcoin)
			.supported_quantity(Quantity::Bounded(quantity))
			.metadata(vec![
				0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0xba, 0xbe, 0x12, 0x34, 0x56, 0x78, 0x90, 0xab,
				0xcd, 0xef,
			])
			.unwrap();

		let ldk_offer = builder.build().unwrap();
		let wrapped_offer = Offer::from(ldk_offer.clone());

		(ldk_offer, wrapped_offer)
	}

	fn create_test_refund() -> (LdkRefund, Refund) {
		let payer_key = bitcoin::secp256k1::PublicKey::from_str(
			"02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
		)
		.unwrap();

		let expiry =
			(SystemTime::now() + Duration::from_secs(3600)).duration_since(UNIX_EPOCH).unwrap();

		let builder = RefundBuilder::new("Test refund".to_string().into(), payer_key, 100_000)
			.unwrap()
			.description("Test refund description".to_string())
			.absolute_expiry(expiry)
			.quantity(3)
			.issuer("test_issuer".to_string());

		let ldk_refund = builder.build().unwrap();
		let wrapped_refund = Refund::from(ldk_refund.clone());

		(ldk_refund, wrapped_refund)
	}

	fn create_test_bolt12_invoice() -> (LdkBolt12Invoice, Bolt12Invoice) {
		let invoice_hex = "0020a5b7104b95f17442d6638143ded62b02c2fda98cdf35841713fd0f44b59286560a000e04682cb028502006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f520227105601015821034b4f0765a115caeff6787a8fb2d976c02467a36aea32901539d76473817937c65904546573745a9c00000068000001000003e75203dee4b3e5d48650caf1faadda53ac6e0dc3f509cc5e9c46defb8aeeec14010348e84fab39b226b1e0696cb6fb40bdb293952c184cf02007fa6e983cd311d189004e7bd75ff9ef069642f2abfa5916099e5a16144e1a6d9b4f246624d3b57d2895d5d2e46fe8661e49717d1663ad2c07b023738370a3e44a960f683040b1862fe36e22347c2dbe429c51af377bdbe01ca0e103f295d1678c68b628957a53a820afcc25763cc67b38aca82067bdf52dc68c061a02575d91c01beca64cc09735c395e91d034841d3e61b58948da631192ce556b85b01028e2284ead4ce184981f4d0f387f8d47295d4fa1dab6a6ae3a417550ac1c8b1aa007b38c926212fbf23154c6ff707621d6eedafc4298b133111d90934bb9d5a2103f0c8e4a3f3daa992334aad300677f23b4285db2ee5caf0a0ecc39c6596c3c4e42318040bec46add3626501f6e422be9c791adc81ea5c83ff0bfa91b7d42bcac0ed128a640fe970da584cff80fd5c12a8ea9b546a2d63515343a933daa21c0000000000000000001800000000000000011d24b2dfac5200000000a404682ca218a820a4a878fb352e63673c05eb07e53563fc8022ff039ad4c66e65848a7cde7ee780aa022710ae03020000b02103800fd75bf6b1e7c5f3fab33a372f6599730e0fae7a30fa4e5c8fbc69c3a87981f0403c9a40e6c9d08e12b0a155101d23a170b4f5b38051b0a0a09a794ce49e820f65d50c8fad7518200d3a28331aa5c668a8f7d70206aaf8bea2e8f05f0904b6e033";

		let invoice_bytes = hex_utils::to_vec(invoice_hex).expect("Valid hex string");

		let ldk_invoice =
			LdkBolt12Invoice::try_from(invoice_bytes).expect("Valid Bolt12Invoice bytes");

		let wrapped_invoice = Bolt12Invoice { inner: ldk_invoice.clone() };

		(ldk_invoice, wrapped_invoice)
	}

	#[test]
	fn test_invoice_description_conversion() {
		let hash = "09d08d4865e8af9266f6cc7c0ae23a1d6bf868207cf8f7c5979b9f6ed850dfb0".to_string();
		let description = Bolt11InvoiceDescription::Hash { hash };
		let converted_description =
			lightning_invoice::Bolt11InvoiceDescription::try_from(&description).unwrap();
		let reconverted_description: Bolt11InvoiceDescription = converted_description.into();
		assert_eq!(description, reconverted_description);
	}

	#[test]
	fn test_bolt11_invoice_basic_properties() {
		let (ldk_invoice, wrapped_invoice) = create_test_bolt11_invoice();

		assert_eq!(
			ldk_invoice.payment_hash().to_string(),
			wrapped_invoice.payment_hash().to_string()
		);
		assert_eq!(ldk_invoice.amount_milli_satoshis(), wrapped_invoice.amount_milli_satoshis());

		assert_eq!(
			ldk_invoice.min_final_cltv_expiry_delta(),
			wrapped_invoice.min_final_cltv_expiry_delta()
		);
		assert_eq!(
			ldk_invoice.payment_secret().0.to_vec(),
			wrapped_invoice.payment_secret().0.to_vec()
		);

		assert_eq!(ldk_invoice.network(), wrapped_invoice.network());
		assert_eq!(
			format!("{:?}", ldk_invoice.currency()),
			format!("{:?}", wrapped_invoice.currency())
		);
	}

	#[test]
	fn test_bolt11_invoice_time_related_fields() {
		let (ldk_invoice, wrapped_invoice) = create_test_bolt11_invoice();

		assert_eq!(ldk_invoice.expiry_time().as_secs(), wrapped_invoice.expiry_time_seconds());
		assert_eq!(
			ldk_invoice.duration_until_expiry().as_secs(),
			wrapped_invoice.seconds_until_expiry()
		);
		assert_eq!(
			ldk_invoice.duration_since_epoch().as_secs(),
			wrapped_invoice.seconds_since_epoch()
		);

		let future_time = Duration::from_secs(wrapped_invoice.seconds_since_epoch() + 10000);
		assert!(!ldk_invoice.would_expire(future_time));
		assert!(!wrapped_invoice.would_expire(future_time.as_secs()));
	}

	#[test]
	fn test_bolt11_invoice_description() {
		let (ldk_invoice, wrapped_invoice) = create_test_bolt11_invoice();

		let ldk_description = ldk_invoice.description();
		let wrapped_description = wrapped_invoice.invoice_description();

		match (ldk_description, &wrapped_description) {
			(
				lightning_invoice::Bolt11InvoiceDescriptionRef::Direct(ldk_description),
				Bolt11InvoiceDescription::Direct { description },
			) => {
				assert_eq!(ldk_description.to_string(), *description)
			},
			(
				lightning_invoice::Bolt11InvoiceDescriptionRef::Hash(ldk_hash),
				Bolt11InvoiceDescription::Hash { hash },
			) => {
				assert_eq!(hex_utils::to_string(ldk_hash.0.as_ref()), *hash)
			},
			_ => panic!("Description types don't match"),
		}
	}

	#[test]
	fn test_bolt11_invoice_route_hints() {
		let (ldk_invoice, wrapped_invoice) = create_test_bolt11_invoice();

		let wrapped_route_hints = wrapped_invoice.route_hints();
		let ldk_route_hints = ldk_invoice.route_hints();
		assert_eq!(ldk_route_hints.len(), wrapped_route_hints.len());

		let ldk_hop = &ldk_route_hints[0].0[0];
		let wrapped_hop = &wrapped_route_hints[0][0];
		assert_eq!(ldk_hop.src_node_id, wrapped_hop.src_node_id);
		assert_eq!(ldk_hop.short_channel_id, wrapped_hop.short_channel_id);
		assert_eq!(ldk_hop.cltv_expiry_delta, wrapped_hop.cltv_expiry_delta);
		assert_eq!(ldk_hop.htlc_minimum_msat, wrapped_hop.htlc_minimum_msat);
		assert_eq!(ldk_hop.htlc_maximum_msat, wrapped_hop.htlc_maximum_msat);
		assert_eq!(ldk_hop.fees.base_msat, wrapped_hop.fees.base_msat);
		assert_eq!(ldk_hop.fees.proportional_millionths, wrapped_hop.fees.proportional_millionths);
	}

	#[test]
	fn test_bolt11_invoice_roundtrip() {
		let (ldk_invoice, wrapped_invoice) = create_test_bolt11_invoice();

		let invoice_str = wrapped_invoice.to_string();
		let parsed_invoice: LdkBolt11Invoice = invoice_str.parse().unwrap();
		assert_eq!(
			ldk_invoice.payment_hash().to_byte_array().to_vec(),
			parsed_invoice.payment_hash().to_byte_array().to_vec()
		);
	}

	#[test]
	fn test_offer() {
		let (ldk_offer, wrapped_offer) = create_test_offer();
		match (ldk_offer.description(), wrapped_offer.description()) {
			(Some(ldk_desc), Some(wrapped_desc)) => {
				assert_eq!(ldk_desc.to_string(), wrapped_desc);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK offer had a description but wrapped offer did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped offer had a description but LDK offer did not!");
			},
		}

		match (ldk_offer.amount(), wrapped_offer.amount()) {
			(Some(ldk_amount), Some(wrapped_amount)) => {
				let ldk_amount: OfferAmount = ldk_amount.into();
				assert_eq!(ldk_amount, wrapped_amount);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK offer had an amount but wrapped offer did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped offer had an amount but LDK offer did not!");
			},
		}

		match (ldk_offer.issuer(), wrapped_offer.issuer()) {
			(Some(ldk_issuer), Some(wrapped_issuer)) => {
				assert_eq!(ldk_issuer.to_string(), wrapped_issuer);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK offer had an issuer but wrapped offer did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped offer had an issuer but LDK offer did not!");
			},
		}

		assert_eq!(ldk_offer.is_expired(), wrapped_offer.is_expired());
		assert_eq!(ldk_offer.id(), wrapped_offer.id());
		assert_eq!(ldk_offer.is_valid_quantity(10_000), wrapped_offer.is_valid_quantity(10_000));
		assert_eq!(ldk_offer.expects_quantity(), wrapped_offer.expects_quantity());
		assert_eq!(
			ldk_offer.supports_chain(Network::Bitcoin.chain_hash()),
			wrapped_offer.supports_chain(Network::Bitcoin)
		);
		assert_eq!(
			ldk_offer.chains(),
			wrapped_offer.chains().iter().map(|c| c.chain_hash()).collect::<Vec<_>>()
		);
		match (ldk_offer.metadata(), wrapped_offer.metadata()) {
			(Some(ldk_metadata), Some(wrapped_metadata)) => {
				assert_eq!(ldk_metadata.clone(), wrapped_metadata);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK offer had metadata but wrapped offer did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped offer had metadata but LDK offer did not!");
			},
		}

		match (ldk_offer.absolute_expiry(), wrapped_offer.absolute_expiry_seconds()) {
			(Some(ldk_expiry), Some(wrapped_expiry)) => {
				assert_eq!(ldk_expiry.as_secs(), wrapped_expiry);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK offer had an absolute expiry but wrapped offer did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped offer had an absolute expiry but LDK offer did not!");
			},
		}

		match (ldk_offer.issuer_signing_pubkey(), wrapped_offer.issuer_signing_pubkey()) {
			(Some(ldk_expiry_signing_pubkey), Some(wrapped_issuer_signing_pubkey)) => {
				assert_eq!(ldk_expiry_signing_pubkey, wrapped_issuer_signing_pubkey);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK offer had an issuer signing pubkey but wrapped offer did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped offer had an issuer signing pubkey but LDK offer did not!");
			},
		}
	}

	#[test]
	fn test_refund_roundtrip() {
		let (ldk_refund, _) = create_test_refund();

		let refund_str = ldk_refund.to_string();

		let parsed_refund = Refund::from_str(&refund_str);
		assert!(parsed_refund.is_ok(), "Failed to parse refund from string!");

		let invalid_result = Refund::from_str("invalid_refund_string");
		assert!(invalid_result.is_err());
		assert!(matches!(invalid_result.err().unwrap(), Error::InvalidRefund));
	}

	#[test]
	fn test_refund_properties() {
		let (ldk_refund, wrapped_refund) = create_test_refund();

		assert_eq!(ldk_refund.description().to_string(), wrapped_refund.description());
		assert_eq!(ldk_refund.amount_msats(), wrapped_refund.amount_msats());
		assert_eq!(ldk_refund.is_expired(), wrapped_refund.is_expired());

		match (ldk_refund.absolute_expiry(), wrapped_refund.absolute_expiry_seconds()) {
			(Some(ldk_expiry), Some(wrapped_expiry)) => {
				assert_eq!(ldk_expiry.as_secs(), wrapped_expiry);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK refund had an expiry but wrapped refund did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped refund had an expiry but LDK refund did not!");
			},
		}

		match (ldk_refund.quantity(), wrapped_refund.quantity()) {
			(Some(ldk_expiry), Some(wrapped_expiry)) => {
				assert_eq!(ldk_expiry, wrapped_expiry);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK refund had an quantity but wrapped refund did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped refund had an quantity but LDK refund did not!");
			},
		}

		match (ldk_refund.issuer(), wrapped_refund.issuer()) {
			(Some(ldk_issuer), Some(wrapped_issuer)) => {
				assert_eq!(ldk_issuer.to_string(), wrapped_issuer);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK refund had an issuer but wrapped refund did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped refund had an issuer but LDK refund did not!");
			},
		}

		assert_eq!(ldk_refund.payer_metadata().to_vec(), wrapped_refund.payer_metadata());
		assert_eq!(ldk_refund.payer_signing_pubkey(), wrapped_refund.payer_signing_pubkey());

		if let Ok(network) = Network::try_from(ldk_refund.chain()) {
			assert_eq!(wrapped_refund.chain(), Some(network));
		}

		assert_eq!(ldk_refund.payer_note().map(|p| p.to_string()), wrapped_refund.payer_note());
	}

	#[test]
	fn test_bolt12_invoice_properties() {
		let (ldk_invoice, wrapped_invoice) = create_test_bolt12_invoice();

		assert_eq!(
			ldk_invoice.payment_hash().0.to_vec(),
			wrapped_invoice.payment_hash().0.to_vec()
		);
		assert_eq!(ldk_invoice.amount_msats(), wrapped_invoice.amount_msats());
		assert_eq!(ldk_invoice.is_expired(), wrapped_invoice.is_expired());

		assert_eq!(ldk_invoice.signing_pubkey(), wrapped_invoice.signing_pubkey());

		assert_eq!(ldk_invoice.created_at().as_secs(), wrapped_invoice.created_at());

		match (ldk_invoice.absolute_expiry(), wrapped_invoice.absolute_expiry_seconds()) {
			(Some(ldk_expiry), Some(wrapped_expiry)) => {
				assert_eq!(ldk_expiry.as_secs(), wrapped_expiry);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK invoice had an absolute expiry but wrapped invoice did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped invoice had an absolute expiry but LDK invoice did not!");
			},
		}

		assert_eq!(ldk_invoice.relative_expiry().as_secs(), wrapped_invoice.relative_expiry());

		match (ldk_invoice.description(), wrapped_invoice.description()) {
			(Some(ldk_desc), Some(wrapped_desc)) => {
				assert_eq!(ldk_desc.to_string(), wrapped_desc);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK invoice had a description but wrapped invoice did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped invoice had a description but LDK invoice did not!");
			},
		}

		match (ldk_invoice.issuer(), wrapped_invoice.issuer()) {
			(Some(ldk_issuer), Some(wrapped_issuer)) => {
				assert_eq!(ldk_issuer.to_string(), wrapped_issuer);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK invoice had an issuer but wrapped invoice did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped invoice had an issuer but LDK invoice did not!");
			},
		}

		match (ldk_invoice.payer_note(), wrapped_invoice.payer_note()) {
			(Some(ldk_note), Some(wrapped_note)) => {
				assert_eq!(ldk_note.to_string(), wrapped_note);
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK invoice had a payer note but wrapped invoice did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped invoice had a payer note but LDK invoice did not!");
			},
		}

		match (ldk_invoice.metadata(), wrapped_invoice.metadata()) {
			(Some(ldk_metadata), Some(wrapped_metadata)) => {
				assert_eq!(ldk_metadata.as_slice(), wrapped_metadata.as_slice());
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK invoice had metadata but wrapped invoice did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped invoice had metadata but LDK invoice did not!");
			},
		}

		assert_eq!(ldk_invoice.quantity(), wrapped_invoice.quantity());

		assert_eq!(ldk_invoice.chain().to_bytes().to_vec(), wrapped_invoice.chain());

		match (ldk_invoice.offer_chains(), wrapped_invoice.offer_chains()) {
			(Some(ldk_chains), Some(wrapped_chains)) => {
				assert_eq!(ldk_chains.len(), wrapped_chains.len());
				for (i, ldk_chain) in ldk_chains.iter().enumerate() {
					assert_eq!(ldk_chain.to_bytes().to_vec(), wrapped_chains[i]);
				}
			},
			(None, None) => {
				// Both fields are missing which is expected behaviour when converting
			},
			(Some(_), None) => {
				panic!("LDK invoice had offer chains but wrapped invoice did not!");
			},
			(None, Some(_)) => {
				panic!("Wrapped invoice had offer chains but LDK invoice did not!");
			},
		}

		let ldk_fallbacks = ldk_invoice.fallbacks();
		let wrapped_fallbacks = wrapped_invoice.fallback_addresses();
		assert_eq!(ldk_fallbacks.len(), wrapped_fallbacks.len());
		for (i, ldk_fallback) in ldk_fallbacks.iter().enumerate() {
			assert_eq!(*ldk_fallback, wrapped_fallbacks[i]);
		}

		assert_eq!(ldk_invoice.encode(), wrapped_invoice.encode());

		assert_eq!(ldk_invoice.signable_hash().to_vec(), wrapped_invoice.signable_hash());
	}
}
