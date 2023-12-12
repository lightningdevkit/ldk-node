pub(crate) use lightning::util::logger::Logger;
pub(crate) use lightning::{log_bytes, log_debug, log_error, log_info, log_trace};

use lightning::util::logger::{Level, Record};
use lightning::util::ser::Writer;

use chrono::Utc;

use std::fs;
#[cfg(not(target_os = "windows"))]
use std::os::unix::fs::symlink;
use std::path::Path;

pub(crate) struct FilesystemLogger {
	file_path: String,
	level: Level,
}

impl FilesystemLogger {
	pub(crate) fn new(log_dir: String, level: Level) -> Result<Self, ()> {
		let log_file_name =
			format!("ldk_node_{}.log", chrono::offset::Local::now().format("%Y_%m_%d"));
		let log_file_path = format!("{}/{}", log_dir, log_file_name);

		if let Some(parent_dir) = Path::new(&log_file_path).parent() {
			fs::create_dir_all(parent_dir).expect("Failed to create log parent directory");

			// make sure the file exists, so that the symlink has something to point to.
			fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(log_file_path.clone())
				.map_err(|e| eprintln!("ERROR: Failed to open log file: {}", e))?;

			#[cfg(not(target_os = "windows"))]
			{
				// Create a symlink to the current log file, with prior cleanup
				let log_file_symlink = parent_dir.join("ldk_node_latest.log");
				if log_file_symlink.as_path().is_symlink() {
					fs::remove_file(&log_file_symlink).map_err(|e| {
						eprintln!("ERROR: Failed to remove log file symlink: {}", e)
					})?;
				}
				symlink(&log_file_name, &log_file_symlink)
					.map_err(|e| eprintln!("ERROR: Failed to create log file symlink: {}", e))?;
			}
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
