// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use std::fmt;

/// Represents an error returned by the LDK server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LdkServerError {
	/// The error message containing a generic description of the error condition in English.
	/// It is intended for a human audience only and should not be parsed to extract any information
	/// programmatically. Client-side code may use it for logging only.
	pub message: String,

	/// The error code uniquely identifying an error condition.
	/// It is meant to be read and understood programmatically by code that detects/handles errors by
	/// type.
	pub error_code: LdkServerErrorCode,
}

impl LdkServerError {
	/// Creates a new [`LdkServerError`] with the given error code and message.
	pub fn new(error_code: LdkServerErrorCode, message: impl Into<String>) -> Self {
		Self { error_code, message: message.into() }
	}
}

impl std::error::Error for LdkServerError {}

impl fmt::Display for LdkServerError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "Error: [{}]: {}", self.error_code, self.message)
	}
}

/// Defines error codes for categorizing LDK server errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LdkServerErrorCode {
	/// Please refer to [`ldk_server_protos::error::ErrorCode::InvalidRequestError`].
	InvalidRequestError,

	/// Please refer to [`ldk_server_protos::error::ErrorCode::AuthError`].
	AuthError,

	/// Please refer to [`ldk_server_protos::error::ErrorCode::LightningError`].
	LightningError,

	/// Please refer to [`ldk_server_protos::error::ErrorCode::InternalServerError`].
	InternalServerError,

	/// There is an unknown error, it could be a client-side bug, unrecognized error-code, network error
	/// or something else.
	InternalError,
}

impl fmt::Display for LdkServerErrorCode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			LdkServerErrorCode::InvalidRequestError => write!(f, "InvalidRequestError"),
			LdkServerErrorCode::AuthError => write!(f, "AuthError"),
			LdkServerErrorCode::LightningError => write!(f, "LightningError"),
			LdkServerErrorCode::InternalServerError => write!(f, "InternalServerError"),
			LdkServerErrorCode::InternalError => write!(f, "InternalError"),
		}
	}
}
