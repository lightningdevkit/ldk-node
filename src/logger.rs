// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

pub(crate) use lightning::util::logger::Logger;
pub(crate) use lightning::{log_bytes, log_debug, log_error, log_info, log_trace};

use lightning::util::logger::{Level, Record};

use chrono::Utc;
use log::{debug, error, info, trace, warn};

use std::fmt::Debug;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::{FormatterConfig, WriterType};

/// [`LogWriter`] trait to write/forward/relay logs to different destinations
/// such as the filesystem, and other loggers.
pub trait LogWriter: Debug + Send + Sync {
	/// Write log to destination.
	fn write(&self, level: Level, message: &str);
}

/// [`LdkNodeLogger`] writer variants.
#[derive(Debug)]
pub enum Writer {
	/// Writes logs to filesystem.
	FileWriter { log_file: Mutex<fs::File> },
	/// Relays logs to [`log`] logger.
	LogRelayWriter,
	/// Forwards logs to a custom logger.
	CustomWriter { inner: Arc<dyn LogWriter + Send + Sync> },
}

impl Writer {
	/// Creates a new writer given the writer's type.
	pub fn new(writer_type: &WriterType) -> Result<Self, ()> {
		match &writer_type {
			// Initial logic for Writer that writes directly to a
			// specified file on the filesystem.
			WriterType::File(file_writer_config) => {
				let log_file_path = &file_writer_config.log_file_path;
				if let Some(parent_dir) = Path::new(log_file_path).parent() {
					fs::create_dir_all(parent_dir).map_err(|e| {
						eprintln!("ERROR: Failed to create log file directory: {}", e);
						()
					})?;
				}

				let log_file = Mutex::new(
					fs::OpenOptions::new().create(true).append(true).open(&log_file_path).map_err(
						|e| {
							eprintln!("ERROR: Failed to open log file: {}", e);
							()
						},
					)?,
				);
				let writer = Writer::FileWriter { log_file };

				Ok(writer)
			},
			// Initial logic for Writer that forwards to any logger that
			// implements the `log` facade.
			WriterType::LogRelay(_log_relay_writer_config) => Ok(Writer::LogRelayWriter),
			// Initial logic for Writer that forwards to any custom logger.
			WriterType::Custom(custom_writer_config) => {
				Ok(Writer::CustomWriter { inner: custom_writer_config.inner.clone() })
			},
		}
	}
}

impl LogWriter for Writer {
	fn write(&self, level: Level, message: &str) {
		match self {
			Writer::FileWriter { log_file } => log_file
				.lock()
				.expect("log file lock poisoned")
				.write_all(message.as_bytes())
				.expect("Failed to write to log file"),
			Writer::LogRelayWriter => match level {
				Level::Gossip => {
					// trace!(..) used for gossip logs here.
					trace!("{message}")
				},
				Level::Trace => trace!("{message}"),
				Level::Debug => debug!("{message}"),
				Level::Info => info!("{message}"),
				Level::Warn => warn!("{message}"),
				Level::Error => error!("{message}"),
			},
			Writer::CustomWriter { inner } => {
				inner.write(level, message);
			},
		}
	}
}

pub type Formatter = Box<dyn Fn(&Record) -> String + Send + Sync>;

/// Logger for LDK Node.
pub struct LdkNodeLogger {
	/// Specifies the log level.
	level: Level,
	/// Specifies the logger's formatter.
	formatter: Formatter,
	/// Specifies the logger's writer.
	writer: Writer,
}

impl LdkNodeLogger {
	pub fn new(
		level: Level, formatter: Box<dyn Fn(&Record) -> String + Send + Sync>, writer: Writer,
	) -> Result<Self, ()> {
		Ok(Self { level, formatter, writer })
	}
}

impl Debug for LdkNodeLogger {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		write!(f, "LdkNodeLogger level: {}", self.level)
	}
}

impl Logger for LdkNodeLogger {
	fn log(&self, record: Record) {
		if record.level < self.level {
			return;
		}
		let message = (self.formatter)(&record);
		self.writer.write(self.level, &message)
	}
}

/// Builds a formatter given the formatter's configuration options.
pub(crate) fn build_formatter(formatter_config: FormatterConfig) -> Formatter {
	let fn_closure = move |record: &Record| {
		let raw_log = record.args.to_string();

		let ts_format = formatter_config.timestamp_format.as_deref().unwrap_or("%Y-%m-%d %H:%M:%S");

		let msg_tmpl = formatter_config
			.message_template
			.as_deref()
			.unwrap_or("{timestamp} {level:<5} [{module_path}:{line}] {message}\n");

		let timestamp = if formatter_config.include_timestamp {
			Utc::now().format(&ts_format).to_string()
		} else {
			String::new()
		};

		let level = if formatter_config.include_level {
			format!("{:<5}", record.level)
		} else {
			String::new()
		};

		let mut log = msg_tmpl.to_string();
		log = log
			.replace("{timestamp}", &timestamp)
			.replace("{level}", &level)
			.replace("{module_path}", &record.module_path)
			.replace("{line}", &format!("{}", record.line))
			.replace("{message}", &raw_log);

		log
	};

	Box::new(fn_closure)
}
