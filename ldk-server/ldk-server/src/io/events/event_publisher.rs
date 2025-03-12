use crate::api::error::LdkServerError;
use async_trait::async_trait;
use ldk_server_protos::events::EventEnvelope;

/// A trait for publishing events or notifications from the LDK Server.
///
/// Implementors of this trait define how events are sent to various messaging
/// systems. It provides a consistent, asynchronous interface for event publishing, while allowing
/// each implementation to manage  its own initialization and configuration, typically sourced from
/// the `ldk-server.config` file. A no-op implementation is included by default,
/// with specific implementations enabled via feature flags.
///
/// Events are represented as [`EventEnvelope`] messages, which are Protocol Buffers
/// ([protobuf](https://protobuf.dev/)) objects defined in [`ldk_server_protos::events`].
/// These events are serialized to bytes by the publisher before transmission, and consumers can
/// deserialize them using the protobuf definitions.
///
/// The underlying messaging system is expected to support durably buffered events,
/// enabling easy decoupling between the LDK Server and event consumers.
#[async_trait]
pub trait EventPublisher {
	/// Publishes an event to the underlying messaging system.
	///
	/// # Arguments
	/// * `event` - The event message to publish, provided as an [`EventEnvelope`]
	///             defined in [`ldk_server_protos::events`]. Implementors must serialize
	///             the whole [`EventEnvelope`] to bytes before publishing.
	///
	/// In order to ensure no events are lost, implementors of this trait must publish events
	/// durably to underlying messaging system. An event is considered published when
	/// [`EventPublisher::publish`] returns `Ok(())`, thus implementors MUST durably persist/publish events *before*
	/// returning `Ok(())`.
	///
	/// # Errors
	/// May return an [`LdkServerErrorCode::InternalServerError`] if the event cannot be published,
	/// such as due to network failures, misconfiguration, or transport-specific issues.
	/// If event publishing fails, the LDK Server will retry publishing the event indefinitely, which
	/// may degrade performance until the underlying messaging system is operational again.
	///
	/// [`LdkServerErrorCode::InternalServerError`]: crate::api::error::LdkServerErrorCode
	async fn publish(&self, event: EventEnvelope) -> Result<(), LdkServerError>;
}
