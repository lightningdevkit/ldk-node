pub(crate) mod event_publisher;

#[cfg(feature = "events-rabbitmq")]
pub(crate) mod rabbitmq;

use ldk_server_protos::events::event_envelope;

/// Event variant to event name mapping.
pub(crate) fn get_event_name(event: &event_envelope::Event) -> &'static str {
	match event {
		event_envelope::Event::PaymentReceived(_) => "PaymentReceived",
		event_envelope::Event::PaymentSuccessful(_) => "PaymentSuccessful",
		event_envelope::Event::PaymentFailed(_) => "PaymentFailed",
		event_envelope::Event::PaymentForwarded(_) => "PaymentForwarded",
	}
}
