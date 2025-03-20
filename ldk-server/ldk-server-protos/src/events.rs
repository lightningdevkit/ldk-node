/// EventEnvelope wraps different event types in a single message to be used by EventPublisher.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct EventEnvelope {
	#[prost(oneof = "event_envelope::Event", tags = "2, 3, 4, 6")]
	pub event: ::core::option::Option<event_envelope::Event>,
}
/// Nested message and enum types in `EventEnvelope`.
pub mod event_envelope {
	#[allow(clippy::derive_partial_eq_without_eq)]
	#[derive(Clone, PartialEq, ::prost::Oneof)]
	pub enum Event {
		#[prost(message, tag = "2")]
		PaymentReceived(super::PaymentReceived),
		#[prost(message, tag = "3")]
		PaymentSuccessful(super::PaymentSuccessful),
		#[prost(message, tag = "4")]
		PaymentFailed(super::PaymentFailed),
		#[prost(message, tag = "6")]
		PaymentForwarded(super::PaymentForwarded),
	}
}
/// PaymentReceived indicates a payment has been received.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaymentReceived {
	/// The payment details for the payment in event.
	#[prost(message, optional, tag = "1")]
	pub payment: ::core::option::Option<super::types::Payment>,
}
/// PaymentSuccessful indicates a sent payment was successful.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaymentSuccessful {
	/// The payment details for the payment in event.
	#[prost(message, optional, tag = "1")]
	pub payment: ::core::option::Option<super::types::Payment>,
}
/// PaymentFailed indicates a sent payment has failed.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaymentFailed {
	/// The payment details for the payment in event.
	#[prost(message, optional, tag = "1")]
	pub payment: ::core::option::Option<super::types::Payment>,
}
/// PaymentForwarded indicates a payment was forwarded through the node.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PaymentForwarded {
	#[prost(message, optional, tag = "1")]
	pub forwarded_payment: ::core::option::Option<super::types::ForwardedPayment>,
}
