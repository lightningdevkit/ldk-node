use crate::api::error::LdkServerError;
use crate::api::error::LdkServerErrorCode::InternalServerError;
use crate::io::events::event_publisher::EventPublisher;
use ::prost::Message;
use async_trait::async_trait;
use lapin::options::{BasicPublishOptions, ConfirmSelectOptions, ExchangeDeclareOptions};
use lapin::types::FieldTable;
use lapin::{
	BasicProperties, Channel, Connection, ConnectionProperties, ConnectionState, ExchangeKind,
};
use ldk_server_protos::events::EventEnvelope;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A RabbitMQ-based implementation of the EventPublisher trait.
pub struct RabbitMqEventPublisher {
	/// The RabbitMQ connection, used for reconnection logic.
	connection: Arc<Mutex<Option<Connection>>>,
	/// The RabbitMQ channel used for publishing events.
	channel: Arc<Mutex<Option<Channel>>>,
	/// Configuration details, including connection string and exchange name.
	config: RabbitMqConfig,
}

/// Configuration for the RabbitMQ event publisher.
#[derive(Debug, Clone)]
pub struct RabbitMqConfig {
	pub connection_string: String,
	pub exchange_name: String,
}

/// Delivery mode for persistent messages (written to disk).
const DELIVERY_MODE_PERSISTENT: u8 = 2;

impl RabbitMqEventPublisher {
	/// Creates a new RabbitMqEventPublisher instance.
	pub fn new(config: RabbitMqConfig) -> Self {
		Self { connection: Arc::new(Mutex::new(None)), channel: Arc::new(Mutex::new(None)), config }
	}

	async fn connect(config: &RabbitMqConfig) -> Result<(Connection, Channel), LdkServerError> {
		let conn = Connection::connect(&config.connection_string, ConnectionProperties::default())
			.await
			.map_err(|e| {
				LdkServerError::new(
					InternalServerError,
					format!("Failed to connect to RabbitMQ: {}", e),
				)
			})?;

		let channel = conn.create_channel().await.map_err(|e| {
			LdkServerError::new(InternalServerError, format!("Failed to create channel: {}", e))
		})?;

		channel.confirm_select(ConfirmSelectOptions::default()).await.map_err(|e| {
			LdkServerError::new(InternalServerError, format!("Failed to enable confirms: {}", e))
		})?;

		channel
			.exchange_declare(
				&config.exchange_name,
				ExchangeKind::Fanout,
				ExchangeDeclareOptions { durable: true, ..Default::default() },
				FieldTable::default(),
			)
			.await
			.map_err(|e| {
				LdkServerError::new(
					InternalServerError,
					format!("Failed to declare exchange: {}", e),
				)
			})?;

		Ok((conn, channel))
	}

	async fn ensure_connected(&self) -> Result<(), LdkServerError> {
		{
			let connection = self.connection.lock().await;
			if let Some(connection) = &*connection {
				if connection.status().state() == ConnectionState::Connected {
					return Ok(());
				}
			}
		}

		// Connection is not alive, attempt reconnecting.
		let (connection, channel) = Self::connect(&self.config)
			.await
			.map_err(|e| LdkServerError::new(InternalServerError, e.to_string()))?;
		*self.connection.lock().await = Some(connection);
		*self.channel.lock().await = Some(channel);
		Ok(())
	}
}

#[async_trait]
impl EventPublisher for RabbitMqEventPublisher {
	/// Publishes an event to RabbitMQ.
	///
	/// The event is published to a fanout exchange with persistent delivery mode,
	/// and the method waits for confirmation from RabbitMQ to ensure durability.
	async fn publish(&self, event: EventEnvelope) -> Result<(), LdkServerError> {
		// Ensure connection is alive before proceeding
		self.ensure_connected().await?;

		let channel_guard = self.channel.lock().await;
		let channel = channel_guard.as_ref().ok_or_else(|| {
			LdkServerError::new(InternalServerError, "Channel not initialized".to_string())
		})?;

		// Publish the event with persistent delivery mode
		let confirm = channel
			.basic_publish(
				&self.config.exchange_name,
				"", // Empty routing key should be used for fanout exchange, since it is ignored.
				BasicPublishOptions::default(),
				&event.encode_to_vec(),
				BasicProperties::default().with_delivery_mode(DELIVERY_MODE_PERSISTENT),
			)
			.await
			.map_err(|e| {
				LdkServerError::new(
					InternalServerError,
					format!("Failed to publish event, error: {}", e),
				)
			})?;

		let confirmation = confirm.await.map_err(|e| {
			LdkServerError::new(InternalServerError, format!("Failed to get confirmation: {}", e))
		})?;

		match confirmation {
			lapin::publisher_confirm::Confirmation::Ack(_) => Ok(()),
			lapin::publisher_confirm::Confirmation::Nack(_) => Err(LdkServerError::new(
				InternalServerError,
				"Message not acknowledged".to_string(),
			)),
			_ => {
				Err(LdkServerError::new(InternalServerError, "Unexpected confirmation".to_string()))
			},
		}
	}
}
