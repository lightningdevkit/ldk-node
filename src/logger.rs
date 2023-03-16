pub(crate) use lightning::util::logger::Logger;
use lightning::util::logger::Record;
use lightning::util::ser::Writer;
pub(crate) use lightning::{log_error, log_info, log_trace};

use chrono::Utc;

use std::fs;
use std::path::Path;

pub(crate) struct FilesystemLogger {
	file_path: String,
}

impl FilesystemLogger {
	pub(crate) fn new(file_path: String) -> Self {
		if let Some(parent_dir) = Path::new(&file_path).parent() {
			fs::create_dir_all(parent_dir).unwrap();
		}
		Self { file_path }
	}
}
impl Logger for FilesystemLogger {
	fn log(&self, record: &Record) {
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
			.unwrap()
			.write_all(log.as_bytes())
			.unwrap();
	}
}
