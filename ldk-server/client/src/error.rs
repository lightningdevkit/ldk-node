use prost::DecodeError;

/// When there is an error in request to LDK Node Server, the response contains a relevant error code.
#[derive(Debug)]
pub enum LdkNodeServerError {
	/// There is an unknown error. (Placeholder until error handling is done.)
	InternalError(String),
}

impl From<DecodeError> for LdkNodeServerError {
	fn from(err: DecodeError) -> Self {
		LdkNodeServerError::InternalError(err.to_string())
	}
}

impl From<reqwest::Error> for LdkNodeServerError {
	fn from(err: reqwest::Error) -> Self {
		LdkNodeServerError::InternalError(err.to_string())
	}
}
