// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::fmt;

use bdk_chain::bitcoin::psbt::ExtractTxError as BdkExtractTxError;
use bdk_chain::local_chain::CannotConnectError as BdkChainConnectionError;
use bdk_chain::tx_graph::CalculateFeeError as BdkChainCalculateFeeError;
use bdk_wallet::error::CreateTxError as BdkCreateTxError;
#[allow(deprecated)]
use bdk_wallet::signer::SignerError as BdkSignerError;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// An error that possibly needs to be handled by the user.
pub enum Error {
	/// Returned when trying to start [`crate::Node`] while it is already running.
	AlreadyRunning,
	/// Returned when trying to stop [`crate::Node`] while it is not running.
	NotRunning,
	/// An on-chain transaction could not be created.
	OnchainTxCreationFailed,
	/// A network connection has been closed.
	ConnectionFailed,
	/// Invoice creation failed.
	InvoiceCreationFailed,
	/// Invoice request creation failed.
	InvoiceRequestCreationFailed,
	/// Offer creation failed.
	OfferCreationFailed,
	/// Refund creation failed.
	RefundCreationFailed,
	/// Sending a payment has failed.
	PaymentSendingFailed,
	/// Sending of spontaneous payment with custom TLVs failed.
	InvalidCustomTlvs,
	/// Sending a payment probe has failed.
	ProbeSendingFailed,
	/// A channel could not be opened.
	ChannelCreationFailed,
	/// A channel could not be closed.
	ChannelClosingFailed,
	/// A channel could not be spliced.
	ChannelSplicingFailed,
	/// A channel configuration could not be updated.
	ChannelConfigUpdateFailed,
	/// Persistence failed.
	PersistenceFailed,
	/// A fee rate estimation update failed.
	FeerateEstimationUpdateFailed,
	/// A fee rate estimation update timed out.
	FeerateEstimationUpdateTimeout,
	/// A wallet operation failed.
	WalletOperationFailed,
	/// A wallet operation timed out.
	WalletOperationTimeout,
	/// A signing operation for transaction failed.
	OnchainTxSigningFailed,
	/// A transaction sync operation failed.
	TxSyncFailed,
	/// A transaction sync operation timed out.
	TxSyncTimeout,
	/// A gossip updating operation failed.
	GossipUpdateFailed,
	/// A gossip updating operation timed out.
	GossipUpdateTimeout,
	/// A liquidity request operation failed.
	LiquidityRequestFailed,
	/// Parsing a URI parameter has failed.
	UriParameterParsingFailed,
	/// The given address is invalid.
	InvalidAddress,
	/// The given network address is invalid.
	InvalidSocketAddress,
	/// The given public key is invalid.
	InvalidPublicKey,
	/// The given secret key is invalid.
	InvalidSecretKey,
	/// The given offer id is invalid.
	InvalidOfferId,
	/// The given node id is invalid.
	InvalidNodeId,
	/// The given payment id is invalid.
	InvalidPaymentId,
	/// The given payment hash is invalid.
	InvalidPaymentHash,
	/// The given payment pre-image is invalid.
	InvalidPaymentPreimage,
	/// The given payment secret is invalid.
	InvalidPaymentSecret,
	/// The given amount is invalid.
	InvalidAmount,
	/// The given invoice is invalid.
	InvalidInvoice,
	/// The given offer is invalid.
	InvalidOffer,
	/// The given refund is invalid.
	InvalidRefund,
	/// The given channel ID is invalid.
	InvalidChannelId,
	/// The given network is invalid.
	InvalidNetwork,
	/// The given URI is invalid.
	InvalidUri,
	/// The given quantity is invalid.
	InvalidQuantity,
	/// The given node alias is invalid.
	InvalidNodeAlias,
	/// The given date time is invalid.
	InvalidDateTime,
	/// The given fee rate is invalid.
	InvalidFeeRate,
	/// A payment with the given hash has already been initiated.
	DuplicatePayment,
	/// The provided offer was denonminated in an unsupported currency.
	UnsupportedCurrency,
	/// The available funds are insufficient to complete the given operation.
	InsufficientFunds,
	/// The given operation failed due to the required liquidity source being unavailable.
	LiquiditySourceUnavailable,
	/// The given operation failed due to the LSP's required opening fee being too high.
	LiquidityFeeTooHigh,
	/// The given blinded paths are invalid.
	InvalidBlindedPaths,
	/// Asynchronous payment services are disabled.
	AsyncPaymentServicesDisabled,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Self::AlreadyRunning => write!(f, "Node is already running."),
			Self::NotRunning => write!(f, "Node is not running."),
			Self::OnchainTxCreationFailed => {
				write!(f, "On-chain transaction could not be created.")
			},
			Self::ConnectionFailed => write!(f, "Network connection closed."),
			Self::InvoiceCreationFailed => write!(f, "Failed to create invoice."),
			Self::InvoiceRequestCreationFailed => write!(f, "Failed to create invoice request."),
			Self::OfferCreationFailed => write!(f, "Failed to create offer."),
			Self::RefundCreationFailed => write!(f, "Failed to create refund."),
			Self::PaymentSendingFailed => write!(f, "Failed to send the given payment."),
			Self::InvalidCustomTlvs => write!(f, "Failed to construct payment with custom TLVs."),
			Self::ProbeSendingFailed => write!(f, "Failed to send the given payment probe."),
			Self::ChannelCreationFailed => write!(f, "Failed to create channel."),
			Self::ChannelClosingFailed => write!(f, "Failed to close channel."),
			Self::ChannelSplicingFailed => write!(f, "Failed to splice channel."),
			Self::ChannelConfigUpdateFailed => write!(f, "Failed to update channel config."),
			Self::PersistenceFailed => write!(f, "Failed to persist data."),
			Self::FeerateEstimationUpdateFailed => {
				write!(f, "Failed to update fee rate estimates.")
			},
			Self::FeerateEstimationUpdateTimeout => {
				write!(f, "Updating fee rate estimates timed out.")
			},
			Self::WalletOperationFailed => write!(f, "Failed to conduct wallet operation."),
			Self::WalletOperationTimeout => write!(f, "A wallet operation timed out."),
			Self::OnchainTxSigningFailed => write!(f, "Failed to sign given transaction."),
			Self::TxSyncFailed => write!(f, "Failed to sync transactions."),
			Self::TxSyncTimeout => write!(f, "Syncing transactions timed out."),
			Self::GossipUpdateFailed => write!(f, "Failed to update gossip data."),
			Self::GossipUpdateTimeout => write!(f, "Updating gossip data timed out."),
			Self::LiquidityRequestFailed => write!(f, "Failed to request inbound liquidity."),
			Self::UriParameterParsingFailed => write!(f, "Failed to parse a URI parameter."),
			Self::InvalidAddress => write!(f, "The given address is invalid."),
			Self::InvalidSocketAddress => write!(f, "The given network address is invalid."),
			Self::InvalidPublicKey => write!(f, "The given public key is invalid."),
			Self::InvalidSecretKey => write!(f, "The given secret key is invalid."),
			Self::InvalidOfferId => write!(f, "The given offer id is invalid."),
			Self::InvalidNodeId => write!(f, "The given node id is invalid."),
			Self::InvalidPaymentId => write!(f, "The given payment id is invalid."),
			Self::InvalidPaymentHash => write!(f, "The given payment hash is invalid."),
			Self::InvalidPaymentPreimage => write!(f, "The given payment preimage is invalid."),
			Self::InvalidPaymentSecret => write!(f, "The given payment secret is invalid."),
			Self::InvalidAmount => write!(f, "The given amount is invalid."),
			Self::InvalidInvoice => write!(f, "The given invoice is invalid."),
			Self::InvalidOffer => write!(f, "The given offer is invalid."),
			Self::InvalidRefund => write!(f, "The given refund is invalid."),
			Self::InvalidChannelId => write!(f, "The given channel ID is invalid."),
			Self::InvalidNetwork => write!(f, "The given network is invalid."),
			Self::InvalidUri => write!(f, "The given URI is invalid."),
			Self::InvalidQuantity => write!(f, "The given quantity is invalid."),
			Self::InvalidNodeAlias => write!(f, "The given node alias is invalid."),
			Self::InvalidDateTime => write!(f, "The given date time is invalid."),
			Self::InvalidFeeRate => write!(f, "The given fee rate is invalid."),
			Self::DuplicatePayment => {
				write!(f, "A payment with the given hash has already been initiated.")
			},
			Self::InsufficientFunds => {
				write!(f, "The available funds are insufficient to complete the given operation.")
			},
			Self::UnsupportedCurrency => {
				write!(f, "The provided offer was denonminated in an unsupported currency.")
			},
			Self::LiquiditySourceUnavailable => {
				write!(f, "The given operation failed due to the required liquidity source being unavailable.")
			},
			Self::LiquidityFeeTooHigh => {
				write!(f, "The given operation failed due to the LSP's required opening fee being too high.")
			},
			Self::InvalidBlindedPaths => write!(f, "The given blinded paths are invalid."),
			Self::AsyncPaymentServicesDisabled => {
				write!(f, "Asynchronous payment services are disabled.")
			},
		}
	}
}

impl std::error::Error for Error {}

#[allow(deprecated)]
impl From<BdkSignerError> for Error {
	fn from(_: BdkSignerError) -> Self {
		Self::OnchainTxSigningFailed
	}
}

impl From<BdkCreateTxError> for Error {
	fn from(e: BdkCreateTxError) -> Self {
		match e {
			BdkCreateTxError::CoinSelection(_) => Self::InsufficientFunds,
			_ => Self::OnchainTxCreationFailed,
		}
	}
}

impl From<BdkExtractTxError> for Error {
	fn from(_: BdkExtractTxError) -> Self {
		Self::OnchainTxCreationFailed
	}
}

impl From<BdkChainConnectionError> for Error {
	fn from(_: BdkChainConnectionError) -> Self {
		Self::WalletOperationFailed
	}
}

impl From<BdkChainCalculateFeeError> for Error {
	fn from(_: BdkChainCalculateFeeError) -> Self {
		Self::WalletOperationFailed
	}
}

impl From<lightning_transaction_sync::TxSyncError> for Error {
	fn from(_e: lightning_transaction_sync::TxSyncError) -> Self {
		Self::TxSyncFailed
	}
}
