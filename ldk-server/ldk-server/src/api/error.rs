use ldk_node::NodeError;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LdkServerError {
	// The error message containing a generic description of the error condition in English.
	// It is intended for a human audience only and should not be parsed to extract any information
	// programmatically. Client-side code may use it for logging only.
	pub(crate) message: String,

	// The error code uniquely identifying an error condition.
	// It is meant to be read and understood programmatically by code that detects/handles errors by
	// type.
	pub(crate) error_code: LdkServerErrorCode,
}

impl LdkServerError {
	pub(crate) fn new(error_code: LdkServerErrorCode, message: impl Into<String>) -> Self {
		Self { error_code, message: message.into() }
	}
}

impl std::error::Error for LdkServerError {}

impl fmt::Display for LdkServerError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Error: [{}]: {}", self.error_code, self.message)
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LdkServerErrorCode {
	/// Please refer to [`protos::error::ErrorCode::InvalidRequestError`].
	InvalidRequestError,

	/// Please refer to [`protos::error::ErrorCode::AuthError`].
	AuthError,

	/// Please refer to [`protos::error::ErrorCode::LightningError`].
	LightningError,

	/// Please refer to [`protos::error::ErrorCode::InternalServerError`].
	InternalServerError,
}

impl fmt::Display for LdkServerErrorCode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			LdkServerErrorCode::InvalidRequestError => write!(f, "InvalidRequestError"),
			LdkServerErrorCode::AuthError => write!(f, "AuthError"),
			LdkServerErrorCode::LightningError => write!(f, "LightningError"),
			LdkServerErrorCode::InternalServerError => write!(f, "InternalServerError"),
		}
	}
}

impl From<NodeError> for LdkServerError {
	fn from(error: NodeError) -> Self {
		let (message, error_code) = match error {
			NodeError::InvalidAddress
			| NodeError::InvalidSocketAddress
			| NodeError::InvalidPublicKey
			| NodeError::InvalidSecretKey
			| NodeError::InvalidOfferId
			| NodeError::InvalidNodeId
			| NodeError::InvalidPaymentId
			| NodeError::InvalidPaymentHash
			| NodeError::InvalidPaymentPreimage
			| NodeError::InvalidPaymentSecret
			| NodeError::InvalidAmount
			| NodeError::InvalidInvoice
			| NodeError::InvalidOffer
			| NodeError::InvalidRefund
			| NodeError::InvalidChannelId
			| NodeError::InvalidNetwork
			| NodeError::InvalidUri
			| NodeError::InvalidQuantity
			| NodeError::InvalidNodeAlias
			| NodeError::InvalidDateTime
			| NodeError::InvalidFeeRate
			| NodeError::UriParameterParsingFailed
			| NodeError::InvalidBlindedPaths
			| NodeError::AsyncPaymentServicesDisabled => {
				(error.to_string(), LdkServerErrorCode::InvalidRequestError)
			},

			NodeError::ConnectionFailed
			| NodeError::InvoiceCreationFailed
			| NodeError::InvoiceRequestCreationFailed
			| NodeError::OfferCreationFailed
			| NodeError::RefundCreationFailed
			| NodeError::PaymentSendingFailed
			| NodeError::InvalidCustomTlvs
			| NodeError::ProbeSendingFailed
			| NodeError::ChannelCreationFailed
			| NodeError::ChannelClosingFailed
			| NodeError::ChannelSplicingFailed
			| NodeError::ChannelConfigUpdateFailed
			| NodeError::DuplicatePayment
			| NodeError::InsufficientFunds
			| NodeError::UnsupportedCurrency
			| NodeError::LiquidityFeeTooHigh => (error.to_string(), LdkServerErrorCode::LightningError),

			NodeError::AlreadyRunning
			| NodeError::NotRunning
			| NodeError::PersistenceFailed
			| NodeError::FeerateEstimationUpdateFailed
			| NodeError::FeerateEstimationUpdateTimeout
			| NodeError::WalletOperationFailed
			| NodeError::WalletOperationTimeout
			| NodeError::GossipUpdateFailed
			| NodeError::GossipUpdateTimeout
			| NodeError::LiquiditySourceUnavailable
			| NodeError::LiquidityRequestFailed
			| NodeError::OnchainTxCreationFailed
			| NodeError::OnchainTxSigningFailed
			| NodeError::TxSyncFailed
			| NodeError::TxSyncTimeout => (error.to_string(), LdkServerErrorCode::InternalServerError),
		};
		LdkServerError::new(error_code, message)
	}
}
