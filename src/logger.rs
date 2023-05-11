pub(crate) use lightning::util::logger::Logger;
pub(crate) use lightning::{log_error, log_info, log_trace};

use lightning::util::logger::{Level, Record};
use lightning::util::ser::Writer;

use chrono::Utc;

use std::fs;
use std::path::Path;

pub(crate) struct FilesystemLogger {
	file_path: String,
	level: Level,
}

impl FilesystemLogger {
	pub(crate) fn new(file_path: String, level: Level) -> Self {
		if let Some(parent_dir) = Path::new(&file_path).parent() {
			fs::create_dir_all(parent_dir).expect("Failed to create log parent directory");
		}
		Self { file_path, level }
	}
}
impl Logger for FilesystemLogger {
	fn log(&self, record: &Record) {
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
