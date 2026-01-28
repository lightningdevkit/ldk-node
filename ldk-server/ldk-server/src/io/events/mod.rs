// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

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
