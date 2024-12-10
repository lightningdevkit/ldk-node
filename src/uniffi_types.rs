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
	default_config, AnchorChannelsConfig, EsploraSyncConfig, MaxDustHTLCExposure,
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

pub use lightning_invoice::{Bolt11Invoice, Description};

pub use lightning_liquidity::lsps1::msgs::ChannelInfo as ChannelOrderInfo;
pub use lightning_liquidity::lsps1::msgs::{
	Bolt11PaymentInfo, OrderId, OrderParameters, PaymentState,
};

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
use lightning_invoice::SignedRawBolt11Invoice;

use std::convert::TryInto;
use std::str::FromStr;

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

impl UniffiCustomTypeConverter for Bolt11Invoice {
	type Builtin = String;

	fn into_custom(val: Self::Builtin) -> uniffi::Result<Self> {
		if let Ok(signed) = val.parse::<SignedRawBolt11Invoice>() {
			if let Ok(invoice) = Bolt11Invoice::from_signed(signed) {
				return Ok(invoice);
			}
		}

		Err(Error::InvalidInvoice.into())
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
	#[test]
	fn test_invoice_description_conversion() {
		let hash = "09d08d4865e8af9266f6cc7c0ae23a1d6bf868207cf8f7c5979b9f6ed850dfb0".to_string();
		let description = Bolt11InvoiceDescription::Hash { hash };
		let converted_description =
			lightning_invoice::Bolt11InvoiceDescription::try_from(&description).unwrap();
		let reconverted_description: Bolt11InvoiceDescription = converted_description.into();
		assert_eq!(description, reconverted_description);
	}
}
