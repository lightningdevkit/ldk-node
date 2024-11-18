// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

pub(crate) use lightning::util::logger::{Logger as LdkLogger, Record};
pub(crate) use lightning::{log_bytes, log_debug, log_error, log_info, log_trace};

pub use lightning::util::logger::Level as LdkLevel;

use chrono::Utc;

use std::fs;
use std::io::Write;
use std::path::Path;

/// A unit of logging output with Metadata to enable filtering Module_path,
/// file, line to inform on log's source.
pub struct LogRecord {
	/// The verbosity level of the message.
	pub level: LdkLevel,
	/// The message body.
	pub args: String,
	/// The module path of the message.
	pub module_path: String,
	/// The line containing the message.
	pub line: u32,
}

impl<'a> From<Record<'a>> for LogRecord {
	fn from(record: Record) -> Self {
		Self {
			level: record.level,
			args: record.args.to_string(),
			module_path: record.module_path.to_string(),
			line: record.line,
		}
	}
}

/// LogWriter trait encapsulating the operations required of a
/// logger's writer.
pub trait LogWriter: Send + Sync {
	/// Log the record.
	fn log(&self, record: LogRecord);
}

pub(crate) struct FilesystemLogger {
	file_path: String,
	level: LdkLevel,
}

/// Defines a writer for [`Logger`].
pub(crate) enum Writer {
	/// Writes logs to the file system.
	FileWriter(FilesystemLogger),
}

impl LogWriter for Writer {
	fn log(&self, record: LogRecord) {
		let raw_log = record.args.to_string();
		let log = format!(
			"{} {:<5} [{}:{}] {}\n",
			Utc::now().format("%Y-%m-%d %H:%M:%S"),
			record.level.to_string(),
			record.module_path,
			record.line,
			raw_log
		);

		match self {
			Writer::FileWriter(fs_logger) => {
				if record.level < fs_logger.level {
					return;
				}

				fs::OpenOptions::new()
					.create(true)
					.append(true)
					.open(fs_logger.file_path.clone())
					.expect("Failed to open log file")
					.write_all(log.as_bytes())
					.expect("Failed to write to log file")
			},
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
	pub fn new_fs_writer(log_file_path: String, level: LdkLevel) -> Result<Self, ()> {
		if let Some(parent_dir) = Path::new(&log_file_path).parent() {
			fs::create_dir_all(parent_dir)
				.map_err(|e| eprintln!("ERROR: Failed to create log parent directory: {}", e))?;

			// make sure the file exists.
			fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(&log_file_path)
				.map_err(|e| eprintln!("ERROR: Failed to open log file: {}", e))?;
		}

		let fs_writer = FilesystemLogger { file_path: log_file_path, level };

		Ok(Self { writer: Writer::FileWriter(fs_writer) })
	}
}

impl LdkLogger for Logger {
	fn log(&self, record: Record) {
		self.writer.log(record.into());
	}
}
