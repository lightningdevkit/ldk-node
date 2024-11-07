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

use std::fs;
use std::io::Write;
use std::path::Path;

pub(crate) struct FilesystemLogger {
	file_path: String,
	level: Level,
}

impl FilesystemLogger {
	/// Creates a new filesystem logger given the path to the log file and the log level.
	pub(crate) fn new(log_file_path: String, level: Level) -> Result<Self, ()> {
		if let Some(parent_dir) = Path::new(&log_file_path).parent() {
			fs::create_dir_all(parent_dir).expect("Failed to create log parent directory");

			// make sure the file exists.
			fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(&log_file_path)
				.map_err(|e| eprintln!("ERROR: Failed to open log file: {}", e))?;
		}

		Ok(Self { file_path: log_file_path, level })
	}
}
impl Logger for FilesystemLogger {
	fn log(&self, record: Record) {
		if record.level < self.level {
			return;
		}
		let raw_log = record.args.to_string();
		let log = format!(
			"{} {:<5} [{}:{}] {}\n",
			Utc::now().format("%Y-%m-%d %H:%M:%S"),
			record.level.to_string(),
			record.module_path,
			record.line,
			raw_log
		);
		fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(self.file_path.clone())
			.expect("Failed to open log file")
			.write_all(log.as_bytes())
			.expect("Failed to write to log file")
	}
}
