use std::fmt;

#[derive(Debug, PartialEq, Eq)]
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
	/// An attempted payment has failed.
	PaymentFailed,
	/// A channel could not be opened.
	ChannelCreationFailed,
	/// A channel could not be closed.
	ChannelClosingFailed,
	/// Persistence failed.
	PersistenceFailed,
	/// A wallet operation failed.
	WalletOperationFailed,
	/// A signing operation for transaction failed.
	OnchainTxSigningFailed,
	/// A signing operation for message failed.
	MessageSigningFailed,
	/// A transaction sync operation failed.
	TxSyncFailed,
	/// A gossip updating operation failed.
	GossipUpdateFailed,
	/// The given address is invalid.
	InvalidAddress,
	/// The given network address is invalid.
	InvalidNetAddress,
	/// The given public key is invalid.
	InvalidPublicKey,
	/// The given secret key is invalid.
	InvalidSecretKey,
	/// The given payment hash is invalid.
	InvalidPaymentHash,
	/// The given payment preimage is invalid.
	InvalidPaymentPreimage,
	/// The given payment secret is invalid.
	InvalidPaymentSecret,
	/// The given amount is invalid.
	InvalidAmount,
	/// The given invoice is invalid.
	InvalidInvoice,
	/// The given channel ID is invalid.
	InvalidChannelId,
	/// The given network is invalid.
	InvalidNetwork,
	/// Payment of the given invoice has already been intiated.
	NonUniquePaymentHash,
	/// There are insufficient funds to complete the given operation.
	InsufficientFunds,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Self::AlreadyRunning => write!(f, "Node is already running."),
			Self::NotRunning => write!(f, "Node is not running."),
			Self::OnchainTxCreationFailed => {
				write!(f, "On-chain transaction could not be created.")
			}
			Self::ConnectionFailed => write!(f, "Network connection closed."),
			Self::InvoiceCreationFailed => write!(f, "Failed to create invoice."),
			Self::PaymentFailed => write!(f, "Failed to send the given payment."),
			Self::ChannelCreationFailed => write!(f, "Failed to create channel."),
			Self::ChannelClosingFailed => write!(f, "Failed to close channel."),
			Self::PersistenceFailed => write!(f, "Failed to persist data."),
			Self::WalletOperationFailed => write!(f, "Failed to conduct wallet operation."),
			Self::OnchainTxSigningFailed => write!(f, "Failed to sign given transaction."),
			Self::MessageSigningFailed => write!(f, "Failed to sign given message."),
			Self::TxSyncFailed => write!(f, "Failed to sync transactions."),
			Self::GossipUpdateFailed => write!(f, "Failed to update gossip data."),
			Self::InvalidAddress => write!(f, "The given address is invalid."),
			Self::InvalidNetAddress => write!(f, "The given network address is invalid."),
			Self::InvalidPublicKey => write!(f, "The given public key is invalid."),
			Self::InvalidSecretKey => write!(f, "The given secret key is invalid."),
			Self::InvalidPaymentHash => write!(f, "The given payment hash is invalid."),
			Self::InvalidPaymentPreimage => write!(f, "The given payment preimage is invalid."),
			Self::InvalidPaymentSecret => write!(f, "The given payment secret is invalid."),
			Self::InvalidAmount => write!(f, "The given amount is invalid."),
			Self::InvalidInvoice => write!(f, "The given invoice is invalid."),
			Self::InvalidChannelId => write!(f, "The given channel ID is invalid."),
			Self::InvalidNetwork => write!(f, "The given network is invalid."),
			Self::NonUniquePaymentHash => write!(f, "An invoice must not get payed twice."),
			Self::InsufficientFunds => {
				write!(f, "There are insufficient funds to complete the given operation.")
			}
		}
	}
}

impl std::error::Error for Error {}

impl From<bdk::Error> for Error {
	fn from(e: bdk::Error) -> Self {
		match e {
			bdk::Error::Signer(_) => Self::OnchainTxSigningFailed,
			_ => Self::WalletOperationFailed,
		}
	}
}

impl From<lightning_transaction_sync::TxSyncError> for Error {
	fn from(_e: lightning_transaction_sync::TxSyncError) -> Self {
		Self::TxSyncFailed
	}
}
