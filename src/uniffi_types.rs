pub use crate::payment::payment_store::{
	LSPFeeLimits, PaymentDirection, PaymentKind, PaymentStatus,
};

pub use lightning::events::{ClosureReason, PaymentFailureReason};
pub use lightning::ln::{ChannelId, PaymentHash, PaymentPreimage, PaymentSecret};
pub use lightning::offers::offer::Offer;
pub use lightning::util::string::UntrustedString;

pub use lightning_invoice::Bolt11Invoice;

pub use bitcoin::{Address, BlockHash, Network, OutPoint, Txid};

pub use bip39::Mnemonic;

use crate::UniffiCustomTypeConverter;

use crate::error::Error;
use crate::hex_utils;
use crate::{SocketAddress, UserChannelId};

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::PublicKey;
use lightning::ln::channelmanager::PaymentId;
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
