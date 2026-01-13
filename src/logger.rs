// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Logging-related objects.

use core::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use chrono::Utc;
use lightning::ln::types::ChannelId;
use lightning::types::payment::PaymentHash;
pub use lightning::util::logger::Level as LogLevel;
pub(crate) use lightning::util::logger::{Logger as LdkLogger, Record as LdkRecord};
pub(crate) use lightning::{log_bytes, log_debug, log_error, log_info, log_trace};
use log::{Level as LogFacadeLevel, Record as LogFacadeRecord};

/// A unit of logging output with metadata to enable filtering `module_path`,
/// `file`, and `line` to inform on log's source.
#[cfg(not(feature = "uniffi"))]
pub struct LogRecord<'a> {
	/// The verbosity level of the message.
	pub level: LogLevel,
	/// The message body.
	pub args: fmt::Arguments<'a>,
	/// The module path of the message.
	pub module_path: &'a str,
	/// The line containing the message.
	pub line: u32,
	/// The node id of the peer pertaining to the logged record.
	pub peer_id: Option<PublicKey>,
	/// The channel id of the channel pertaining to the logged record.
	pub channel_id: Option<ChannelId>,
	/// The payment hash pertaining to the logged record.
	pub payment_hash: Option<PaymentHash>,
}

/// Structured context fields for log messages.
///
/// Implements `Display` to format context fields (channel_id, peer_id, payment_hash) directly
/// into a formatter, avoiding intermediate heap allocations when used with `format_args!` or
/// `write!` macros.
///
/// Note: LDK's `Record` Display implementation uses fixed-width padded columns and different
/// formatting for test vs production builds. We intentionally use a simpler format here:
/// fields are only included when present (no padding), and the format is consistent across
/// all build configurations.
pub struct LogContext<'a> {
	/// The channel id of the channel pertaining to the logged record.
	pub channel_id: Option<&'a ChannelId>,
	/// The node id of the peer pertaining to the logged record.
	pub peer_id: Option<&'a PublicKey>,
	/// The payment hash pertaining to the logged record.
	pub payment_hash: Option<&'a PaymentHash>,
}

impl fmt::Display for LogContext<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fn truncate(s: &str) -> &str {
			&s[..s.len().min(6)]
		}

		if self.channel_id.is_none() && self.peer_id.is_none() && self.payment_hash.is_none() {
			return Ok(());
		}

		write!(f, " (")?;
		let mut need_space = false;
		if let Some(c) = self.channel_id {
			write!(f, "ch:{}", truncate(&c.to_string()))?;
			need_space = true;
		}
		if let Some(p) = self.peer_id {
			if need_space {
				write!(f, " ")?;
			}
			write!(f, "p:{}", truncate(&p.to_string()))?;
			need_space = true;
		}
		if let Some(h) = self.payment_hash {
			if need_space {
				write!(f, " ")?;
			}
			write!(f, "h:{}", truncate(&format!("{:?}", h)))?;
		}
		write!(f, ")")
	}
}

/// A unit of logging output with metadata to enable filtering `module_path`,
/// `file`, and `line` to inform on log's source.
///
/// This version is used when the `uniffi` feature is enabled.
/// It is similar to the non-`uniffi` version, but it omits the lifetime parameter
/// for the `LogRecord`, as the Uniffi-exposed interface cannot handle lifetimes.
#[cfg(feature = "uniffi")]
pub struct LogRecord {
	/// The verbosity level of the message.
	pub level: LogLevel,
	/// The message body.
	pub args: String,
	/// The module path of the message.
	pub module_path: String,
	/// The line containing the message.
	pub line: u32,
	/// The node id of the peer pertaining to the logged record.
	pub peer_id: Option<PublicKey>,
	/// The channel id of the channel pertaining to the logged record.
	pub channel_id: Option<ChannelId>,
	/// The payment hash pertaining to the logged record.
	pub payment_hash: Option<PaymentHash>,
}

#[cfg(feature = "uniffi")]
impl<'a> From<LdkRecord<'a>> for LogRecord {
	fn from(record: LdkRecord) -> Self {
		Self {
			level: record.level,
			args: record.args.to_string(),
			module_path: record.module_path.to_string(),
			line: record.line,
			peer_id: record.peer_id,
			channel_id: record.channel_id,
			payment_hash: record.payment_hash,
		}
	}
}

#[cfg(not(feature = "uniffi"))]
impl<'a> From<LdkRecord<'a>> for LogRecord<'a> {
	fn from(record: LdkRecord<'a>) -> Self {
		Self {
			level: record.level,
			args: record.args,
			module_path: record.module_path,
			line: record.line,
			peer_id: record.peer_id,
			channel_id: record.channel_id,
			payment_hash: record.payment_hash,
		}
	}
}

/// Defines the behavior required for writing log records.
///
/// Implementors of this trait are responsible for handling log messages,
/// which may involve formatting, filtering, and forwarding them to specific
/// outputs.
#[cfg(not(feature = "uniffi"))]
pub trait LogWriter: Send + Sync {
	/// Log the record.
	fn log<'a>(&self, record: LogRecord<'a>);
}

/// Defines the behavior required for writing log records.
///
/// Implementors of this trait are responsible for handling log messages,
/// which may involve formatting, filtering, and forwarding them to specific
/// outputs.
/// This version is used when the `uniffi` feature is enabled.
/// It is similar to the non-`uniffi` version, but it omits the lifetime parameter
/// for the `LogRecord`, as the Uniffi-exposed interface cannot handle lifetimes.
#[cfg(feature = "uniffi")]
pub trait LogWriter: Send + Sync {
	/// Log the record.
	fn log(&self, record: LogRecord);
}

/// Defines a writer for [`Logger`].
pub(crate) enum Writer {
	/// Writes logs to the file system.
	FileWriter { file_path: String, max_log_level: LogLevel },
	/// Forwards logs to the `log` facade.
	LogFacadeWriter,
	/// Forwards logs to a custom writer.
	CustomWriter(Arc<dyn LogWriter>),
}

impl LogWriter for Writer {
	fn log(&self, record: LogRecord) {
		let context = LogContext {
			channel_id: record.channel_id.as_ref(),
			peer_id: record.peer_id.as_ref(),
			payment_hash: record.payment_hash.as_ref(),
		};

		match self {
			Writer::FileWriter { file_path, max_log_level } => {
				if record.level < *max_log_level {
					return;
				}

				let log = format!(
					"{} {:<5} [{}:{}] {}{}\n",
					Utc::now().format("%Y-%m-%d %H:%M:%S%.3f"),
					record.level.to_string(),
					record.module_path,
					record.line,
					record.args,
					context,
				);

				fs::OpenOptions::new()
					.create(true)
					.append(true)
					.open(file_path)
					.expect("Failed to open log file")
					.write_all(log.as_bytes())
					.expect("Failed to write to log file")
			},
			Writer::LogFacadeWriter => {
				let mut builder = LogFacadeRecord::builder();

				match record.level {
					LogLevel::Gossip | LogLevel::Trace => builder.level(LogFacadeLevel::Trace),
					LogLevel::Debug => builder.level(LogFacadeLevel::Debug),
					LogLevel::Info => builder.level(LogFacadeLevel::Info),
					LogLevel::Warn => builder.level(LogFacadeLevel::Warn),
					LogLevel::Error => builder.level(LogFacadeLevel::Error),
				};

				#[cfg(not(feature = "uniffi"))]
				log::logger().log(
					&builder
						.target(record.module_path)
						.module_path(Some(record.module_path))
						.line(Some(record.line))
						.args(format_args!("{}{}", record.args, context))
						.build(),
				);
				#[cfg(feature = "uniffi")]
				log::logger().log(
					&builder
						.target(&record.module_path)
						.module_path(Some(&record.module_path))
						.line(Some(record.line))
						.args(format_args!("{}{}", record.args, context))
						.build(),
				);
			},
			Writer::CustomWriter(custom_logger) => custom_logger.log(record),
		}
	}
}

pub(crate) struct Logger {
	/// Specifies the logger's writer.
	writer: Writer,
}

impl Logger {
	/// Creates a new logger with a filesystem writer. The parameters to this function
	/// are the path to the log file, and the log level.
	pub fn new_fs_writer(file_path: String, max_log_level: LogLevel) -> Result<Self, ()> {
		if let Some(parent_dir) = Path::new(&file_path).parent() {
			fs::create_dir_all(parent_dir)
				.map_err(|e| eprintln!("ERROR: Failed to create log parent directory: {}", e))?;

			// make sure the file exists.
			fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(&file_path)
				.map_err(|e| eprintln!("ERROR: Failed to open log file: {}", e))?;
		}

		Ok(Self { writer: Writer::FileWriter { file_path, max_log_level } })
	}

	pub fn new_log_facade() -> Self {
		Self { writer: Writer::LogFacadeWriter }
	}

	pub fn new_custom_writer(log_writer: Arc<dyn LogWriter>) -> Self {
		Self { writer: Writer::CustomWriter(log_writer) }
	}
}

impl LdkLogger for Logger {
	fn log(&self, record: LdkRecord) {
		match &self.writer {
			Writer::FileWriter { file_path: _, max_log_level } => {
				if record.level < *max_log_level {
					return;
				}
				self.writer.log(record.into());
			},
			Writer::LogFacadeWriter => {
				self.writer.log(record.into());
			},
			Writer::CustomWriter(_arc) => {
				self.writer.log(record.into());
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::Mutex;

	/// A minimal log facade logger that captures log output for testing.
	struct TestLogger {
		log: Arc<Mutex<String>>,
	}

	impl log::Log for TestLogger {
		fn enabled(&self, _metadata: &log::Metadata) -> bool {
			true
		}

		fn log(&self, record: &log::Record) {
			*self.log.lock().unwrap() = record.args().to_string();
		}

		fn flush(&self) {}
	}

	/// Tests that LogContext correctly formats all three structured fields
	/// (channel_id, peer_id, payment_hash) with space prefixes and 6-char truncation.
	#[test]
	fn test_log_context_all_fields() {
		let channel_id = ChannelId::from_bytes([
			0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00,
		]);
		let peer_id = PublicKey::from_slice(&[
			0x02, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf1, 0x23, 0x45,
			0x67, 0x89, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf1, 0x23,
			0x45, 0x67, 0x89, 0xab, 0xcd,
		])
		.unwrap();
		let payment_hash = PaymentHash([
			0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00,
		]);

		let context = LogContext {
			channel_id: Some(&channel_id),
			peer_id: Some(&peer_id),
			payment_hash: Some(&payment_hash),
		};

		assert_eq!(context.to_string(), " (ch:abcdef p:02abcd h:fedcba)");
	}

	/// Tests that LogContext returns an empty string when no fields are provided.
	#[test]
	fn test_log_context_no_fields() {
		let context = LogContext { channel_id: None, peer_id: None, payment_hash: None };
		assert_eq!(context.to_string(), "");
	}

	/// Tests that LogContext only includes present fields.
	#[test]
	fn test_log_context_partial_fields() {
		let channel_id = ChannelId::from_bytes([
			0x12, 0x34, 0x56, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00,
		]);

		let context =
			LogContext { channel_id: Some(&channel_id), peer_id: None, payment_hash: None };
		assert_eq!(context.to_string(), " (ch:123456)");
	}

	/// Tests that LogFacadeWriter appends structured context fields to the log message.
	#[test]
	fn test_log_facade_writer_includes_structured_context() {
		let log = Arc::new(Mutex::new(String::new()));
		let test_logger = TestLogger { log: log.clone() };

		let _ = log::set_boxed_logger(Box::new(test_logger));
		log::set_max_level(log::LevelFilter::Trace);

		let writer = Writer::LogFacadeWriter;

		let channel_id = ChannelId::from_bytes([
			0xab, 0xcd, 0xef, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00,
		]);
		let peer_id = PublicKey::from_slice(&[
			0x02, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf1, 0x23, 0x45,
			0x67, 0x89, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf1, 0x23,
			0x45, 0x67, 0x89, 0xab, 0xcd,
		])
		.unwrap();

		#[cfg(not(feature = "uniffi"))]
		let record = LogRecord {
			level: LogLevel::Info,
			args: format_args!("Test message"),
			module_path: "test_module",
			line: 42,
			peer_id: Some(peer_id),
			channel_id: Some(channel_id),
			payment_hash: None,
		};

		#[cfg(feature = "uniffi")]
		let record = LogRecord {
			level: LogLevel::Info,
			args: "Test message".to_string(),
			module_path: "test_module".to_string(),
			line: 42,
			peer_id: Some(peer_id),
			channel_id: Some(channel_id),
			payment_hash: None,
		};

		writer.log(record);

		assert_eq!(*log.lock().unwrap(), "Test message (ch:abcdef p:02abcd)");
	}
}
