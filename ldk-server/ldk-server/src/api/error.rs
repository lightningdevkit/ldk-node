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
