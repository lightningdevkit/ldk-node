// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A logger implementation that writes logs to both stderr and a file.
///
/// The logger formats log messages with RFC3339 timestamps and writes them to:
/// - stdout/stderr for console output
/// - A file specified during initialization
///
/// All log messages follow the format:
/// `[TIMESTAMP LEVEL TARGET FILE:LINE] MESSAGE`
///
/// Example: `[2025-12-04T10:30:45Z INFO ldk_server:42] Starting up...`
///
/// The logger handles SIGHUP for log rotation by reopening the file handle when signaled.
pub struct ServerLogger {
	/// The maximum log level to display
	level: LevelFilter,
	/// The file to write logs to, protected by a mutex for thread-safe access
	file: Mutex<File>,
	/// Path to the log file for reopening on SIGHUP
	log_file_path: PathBuf,
}

impl ServerLogger {
	/// Initializes the global logger with the specified level and file path.
	///
	/// Opens or creates the log file at the given path. If the file exists, logs are appended.
	/// If the file doesn't exist, it will be created along with any necessary parent directories.
	///
	/// This should be called once at application startup. Subsequent calls will fail.
	///
	/// Returns an Arc to the logger for signal handling purposes.
	pub fn init(level: LevelFilter, log_file_path: &Path) -> Result<Arc<Self>, io::Error> {
		// Create parent directories if they don't exist
		if let Some(parent) = log_file_path.parent() {
			fs::create_dir_all(parent)?;
		}

		let file = open_log_file(log_file_path)?;

		let logger = Arc::new(ServerLogger {
			level,
			file: Mutex::new(file),
			log_file_path: log_file_path.to_path_buf(),
		});

		log::set_boxed_logger(Box::new(LoggerWrapper(Arc::clone(&logger))))
			.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
		log::set_max_level(level);
		Ok(logger)
	}

	/// Reopens the log file. Called on SIGHUP for log rotation.
	pub fn reopen(&self) -> Result<(), io::Error> {
		let new_file = open_log_file(&self.log_file_path)?;
		match self.file.lock() {
			Ok(mut file) => {
				// Flush the old buffer before replacing with the new file
				file.flush()?;
				*file = new_file;
				Ok(())
			},
			Err(e) => {
				Err(io::Error::new(io::ErrorKind::Other, format!("Failed to acquire lock: {}", e)))
			},
		}
	}
}

impl Log for ServerLogger {
	fn enabled(&self, metadata: &Metadata) -> bool {
		metadata.level() <= self.level
	}

	fn log(&self, record: &Record) {
		if self.enabled(record.metadata()) {
			let level_str = format_level(record.level());
			let line = record.line().unwrap_or(0);

			// Log to console
			let _ = match record.level() {
				Level::Error => {
					write!(
						io::stderr(),
						"[{} {} {}:{}] {}\n",
						format_timestamp(),
						level_str,
						record.target(),
						line,
						record.args()
					)
				},
				_ => {
					write!(
						io::stdout(),
						"[{} {} {}:{}] {}\n",
						format_timestamp(),
						level_str,
						record.target(),
						line,
						record.args()
					)
				},
			};

			// Log to file
			if let Ok(mut file) = self.file.lock() {
				let _ = write!(
					file,
					"[{} {} {}:{}] {}\n",
					format_timestamp(),
					level_str,
					record.target(),
					line,
					record.args()
				);
			}
		}
	}

	fn flush(&self) {
		let _ = io::stdout().flush();
		let _ = io::stderr().flush();
		if let Ok(mut file) = self.file.lock() {
			let _ = file.flush();
		}
	}
}

fn format_timestamp() -> String {
	let now = chrono::Utc::now();
	now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn format_level(level: Level) -> &'static str {
	match level {
		Level::Error => "ERROR",
		Level::Warn => "WARN ",
		Level::Info => "INFO ",
		Level::Debug => "DEBUG",
		Level::Trace => "TRACE",
	}
}

fn open_log_file(log_file_path: &Path) -> Result<File, io::Error> {
	OpenOptions::new().create(true).append(true).open(log_file_path)
}

/// Wrapper to allow Arc<ServerLogger> to implement Log trait
struct LoggerWrapper(Arc<ServerLogger>);

impl Log for LoggerWrapper {
	fn enabled(&self, metadata: &Metadata) -> bool {
		self.0.enabled(metadata)
	}

	fn log(&self, record: &Record) {
		self.0.log(record)
	}

	fn flush(&self) {
		self.0.flush()
	}
}
