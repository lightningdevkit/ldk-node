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
pub use lightning::offers::invoice::Bolt12Invoice;
pub use lightning::offers::offer::{Offer, OfferId};
pub use lightning::offers::refund::Refund;
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
use lightning::util::ser::Writeable;
use lightning_invoice::{Bolt11Invoice as LdkBolt11Invoice, Bolt11InvoiceDescriptionRef};

use std::convert::TryInto;
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

impl UniffiCustomTypeConverter for Offer {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Offer::from_str(&val).map_err(|_| Error::InvalidOffer.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Refund {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		Refund::from_str(&val).map_err(|_| Error::InvalidRefund.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		obj.to_string()
	}
}

impl UniffiCustomTypeConverter for Bolt12Invoice {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Some(bytes_vec) = hex_utils::to_vec(&val) {
			if let Ok(invoice) = Bolt12Invoice::try_from(bytes_vec) {
				return Ok(invoice);
			}
		}
		Err(Error::InvalidInvoice.into())
	}

	fn from_custom(obj: Self) -> Self::Builtin {
		hex_utils::to_string(&obj.encode())
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

	/// Returns the underlying invoice [`LdkBolt11Invoice`]
	pub fn into_inner(self) -> LdkBolt11Invoice {
		self.inner
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

impl From<Bolt11Invoice> for LdkBolt11Invoice {
	fn from(wrapper: Bolt11Invoice) -> Self {
		wrapper.into_inner()
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
	use super::*;

	fn create_test_invoice() -> (LdkBolt11Invoice, Bolt11Invoice) {
		let invoice_string = "lnbc1pn8g249pp5f6ytj32ty90jhvw69enf30hwfgdhyymjewywcmfjevflg6s4z86qdqqcqzzgxqyz5vqrzjqwnvuc0u4txn35cafc7w94gxvq5p3cu9dd95f7hlrh0fvs46wpvhdfjjzh2j9f7ye5qqqqryqqqqthqqpysp5mm832athgcal3m7h35sc29j63lmgzvwc5smfjh2es65elc2ns7dq9qrsgqu2xcje2gsnjp0wn97aknyd3h58an7sjj6nhcrm40846jxphv47958c6th76whmec8ttr2wmg6sxwchvxmsc00kqrzqcga6lvsf9jtqgqy5yexa";
		let ldk_invoice: LdkBolt11Invoice = invoice_string.parse().unwrap();
		let wrapped_invoice = Bolt11Invoice::from(ldk_invoice.clone());
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
		let (ldk_invoice, wrapped_invoice) = create_test_invoice();

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
		let (ldk_invoice, wrapped_invoice) = create_test_invoice();

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
		let (ldk_invoice, wrapped_invoice) = create_test_invoice();

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
		let (ldk_invoice, wrapped_invoice) = create_test_invoice();

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
		let (ldk_invoice, wrapped_invoice) = create_test_invoice();

		let invoice_str = wrapped_invoice.to_string();
		let parsed_invoice: LdkBolt11Invoice = invoice_str.parse().unwrap();
		assert_eq!(
			ldk_invoice.payment_hash().to_byte_array().to_vec(),
			parsed_invoice.payment_hash().to_byte_array().to_vec()
		);
	}
}
