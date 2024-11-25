use prost::DecodeError;

/// When there is an error in request to LDK Server, the response contains a relevant error code.
#[derive(Debug)]
pub enum LdkServerError {
	/// There is an unknown error. (Placeholder until error handling is done.)
	InternalError(String),
}

impl From<DecodeError> for LdkServerError {
	fn from(err: DecodeError) -> Self {
		LdkServerError::InternalError(err.to_string())
	}
}

impl From<reqwest::Error> for LdkServerError {
	fn from(err: reqwest::Error) -> Self {
		LdkServerError::InternalError(err.to_string())
	}
}
