/// When HttpStatusCode is not ok (200), the response `content` contains a serialized `ErrorResponse`
/// with the relevant ErrorCode and `message`
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ErrorResponse {
	/// The error message containing a generic description of the error condition in English.
	/// It is intended for a human audience only and should not be parsed to extract any information
	/// programmatically. Client-side code may use it for logging only.
	#[prost(string, tag = "1")]
	pub message: ::prost::alloc::string::String,
	/// The error code uniquely identifying an error condition.
	/// It is meant to be read and understood programmatically by code that detects/handles errors by
	/// type.
	///
	/// **Caution**: If a new type of `error_code` is introduced in the `ErrorCode` enum, `error_code` field will be set to
	/// `UnknownError`.
	#[prost(enumeration = "ErrorCode", tag = "2")]
	pub error_code: i32,
}
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum ErrorCode {
	/// Will never be used as `error_code` by server.
	///
	/// **Caution**: If a new type of `error_code` is introduced in the `ErrorCode` enum, `error_code` field will be set to
	/// `UnknownError`.
	UnknownError = 0,
	/// Used in the following cases:
	///    - The request was missing a required argument.
	///    - The specified argument was invalid, incomplete or in the wrong format.
	///    - The request body of api cannot be deserialized into corresponding protobuf object.
	///    - The request does not follow api contract.
	InvalidRequestError = 1,
	/// Used when authentication fails or in case of an unauthorized request.
	AuthError = 2,
	/// Used to represent an error while doing a Lightning operation.
	LightningError = 3,
	/// Used when an internal server error occurred. The client is probably at no fault.
	InternalServerError = 4,
}
impl ErrorCode {
	/// String value of the enum field names used in the ProtoBuf definition.
	///
	/// The values are not transformed in any way and thus are considered stable
	/// (if the ProtoBuf definition does not change) and safe for programmatic use.
	pub fn as_str_name(&self) -> &'static str {
		match self {
			ErrorCode::UnknownError => "UNKNOWN_ERROR",
			ErrorCode::InvalidRequestError => "INVALID_REQUEST_ERROR",
			ErrorCode::AuthError => "AUTH_ERROR",
			ErrorCode::LightningError => "LIGHTNING_ERROR",
			ErrorCode::InternalServerError => "INTERNAL_SERVER_ERROR",
		}
	}
	/// Creates an enum from field names used in the ProtoBuf definition.
	pub fn from_str_name(value: &str) -> ::core::option::Option<Self> {
		match value {
			"UNKNOWN_ERROR" => Some(Self::UnknownError),
			"INVALID_REQUEST_ERROR" => Some(Self::InvalidRequestError),
			"AUTH_ERROR" => Some(Self::AuthError),
			"LIGHTNING_ERROR" => Some(Self::LightningError),
			"INTERNAL_SERVER_ERROR" => Some(Self::InternalServerError),
			_ => None,
		}
	}
}
