use std::fmt;

#[derive(Debug, PartialEq, Eq)]
/// An error that possibly needs to be handled by the user.
pub enum Error {
	/// Returned when trying to start [`crate::Node`] while it is already running.
	AlreadyRunning,
	/// Returned when trying to stop [`crate::Node`] while it is not running.
	NotRunning,
	/// The funding transaction could not be created.
	FundingTxCreationFailed,
	/// A network connection has been closed.
	ConnectionFailed,
	/// Payment of the given invoice has already been intiated.
	NonUniquePaymentHash,
	/// The given amount is invalid.
	InvalidAmount,
	/// The given invoice is invalid.
	InvalidInvoice,
	/// Invoice creation failed.
	InvoiceCreationFailed,
	/// There are insufficient funds to complete the given operation.
	InsufficientFunds,
	/// An attempted payment has failed.
	PaymentFailed,
	/// A given peer info could not be parsed.
	PeerInfoParseFailed,
	/// A channel could not be opened.
	ChannelCreationFailed,
	/// A channel could not be closed.
	ChannelClosingFailed,
	/// Persistence failed.
	PersistenceFailed,
	/// A wallet operation failed.
	WalletOperationFailed,
	/// A siging operation failed.
	WalletSigningFailed,
	/// A transaction sync operation failed.
	TxSyncFailed,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Self::AlreadyRunning => write!(f, "Node is already running."),
			Self::NotRunning => write!(f, "Node is not running."),
			Self::FundingTxCreationFailed => {
				write!(f, "Funding transaction could not be created.")
			}
			Self::ConnectionFailed => write!(f, "Network connection closed."),
			Self::NonUniquePaymentHash => write!(f, "An invoice must not get payed twice."),
			Self::InvalidAmount => write!(f, "The given amount is invalid."),
			Self::InvalidInvoice => write!(f, "The given invoice is invalid."),
			Self::InvoiceCreationFailed => write!(f, "Failed to create invoice."),
			Self::InsufficientFunds => {
				write!(f, "There are insufficient funds to complete the given operation.")
			}
			Self::PaymentFailed => write!(f, "Failed to send the given payment."),
			Self::PeerInfoParseFailed => write!(f, "Failed to parse the given peer information."),
			Self::ChannelCreationFailed => write!(f, "Failed to create channel."),
			Self::ChannelClosingFailed => write!(f, "Failed to close channel."),
			Self::PersistenceFailed => write!(f, "Failed to persist data."),
			Self::WalletOperationFailed => write!(f, "Failed to conduct wallet operation."),
			Self::WalletSigningFailed => write!(f, "Failed to sign given transaction."),
			Self::TxSyncFailed => write!(f, "Failed to sync transactions."),
		}
	}
}

impl std::error::Error for Error {}

impl From<bdk::Error> for Error {
	fn from(e: bdk::Error) -> Self {
		match e {
			bdk::Error::Signer(_) => Self::WalletSigningFailed,
			_ => Self::WalletOperationFailed,
		}
	}
}

impl From<lightning_transaction_sync::TxSyncError> for Error {
	fn from(_e: lightning_transaction_sync::TxSyncError) -> Self {
		Self::TxSyncFailed
	}
}
