pub(crate) use lightning::util::logger::Logger;
use lightning::util::logger::Record;
use lightning::util::ser::Writer;

use chrono::Utc;

use std::fs;

pub(crate) struct FilesystemLogger {
	data_dir: String,
}

impl FilesystemLogger {
	pub(crate) fn new(data_dir: String) -> Self {
		let logs_path = format!("{}/logs", data_dir);
		fs::create_dir_all(logs_path.clone()).unwrap();
		Self { data_dir: logs_path }
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
		let logs_file_path = format!("{}/logs.txt", self.data_dir.clone());
		fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(logs_file_path)
			.unwrap()
			.write_all(log.as_bytes())
			.unwrap();
	}
}


// TODO: We copied the logging macros for now from `lightning::util::macro_logger`. We should
// switch back to using them from upstream after the next release, which includes their export.
macro_rules! log_internal {
	($logger: expr, $lvl:expr, $($arg:tt)+) => (
		$logger.log(&lightning::util::logger::Record::new($lvl, format_args!($($arg)+), module_path!(), file!(), line!()))
	);
}
pub(crate) use log_internal;

macro_rules! log_given_level {
	($logger: expr, $lvl:expr, $($arg:tt)+) => (
		match $lvl {
			#[cfg(not(any(feature = "max_level_off")))]
			lightning::util::logger::Level::Error => log_internal!($logger, $lvl, $($arg)*),
			#[cfg(not(any(feature = "max_level_off", feature = "max_level_error")))]
			lightning::util::logger::Level::Warn => log_internal!($logger, $lvl, $($arg)*),
			#[cfg(not(any(feature = "max_level_off", feature = "max_level_error", feature = "max_level_warn")))]
			lightning::util::logger::Level::Info => log_internal!($logger, $lvl, $($arg)*),
			#[cfg(not(any(feature = "max_level_off", feature = "max_level_error", feature = "max_level_warn", feature = "max_level_info")))]
			lightning::util::logger::Level::Debug => log_internal!($logger, $lvl, $($arg)*),
			#[cfg(not(any(feature = "max_level_off", feature = "max_level_error", feature = "max_level_warn", feature = "max_level_info", feature = "max_level_debug")))]
			lightning::util::logger::Level::Trace => log_internal!($logger, $lvl, $($arg)*),
			#[cfg(not(any(feature = "max_level_off", feature = "max_level_error", feature = "max_level_warn", feature = "max_level_info", feature = "max_level_debug", feature = "max_level_trace")))]
			lightning::util::logger::Level::Gossip => log_internal!($logger, $lvl, $($arg)*),

			#[cfg(any(feature = "max_level_off", feature = "max_level_error", feature = "max_level_warn", feature = "max_level_info", feature = "max_level_debug", feature = "max_level_trace"))]
			_ => {
				// The level is disabled at compile-time
			},
		}
	);
}
pub(crate) use log_given_level;

#[allow(unused_macros)]
macro_rules! log_error {
	($logger: expr, $($arg:tt)*) => (
		log_given_level!($logger, lightning::util::logger::Level::Error, $($arg)*)
	)
}
pub(crate) use log_error;

#[allow(unused_macros)]
macro_rules! log_warn {
	($logger: expr, $($arg:tt)*) => (
		log_given_level!($logger, lightning::util::logger::Level::Warn, $($arg)*)
	)
}
pub(crate) use log_warn;

#[allow(unused_macros)]
macro_rules! log_info {
	($logger: expr, $($arg:tt)*) => (
		log_given_level!($logger, lightning::util::logger::Level::Info, $($arg)*)
	)
}
pub(crate) use log_info;

#[allow(unused_macros)]
macro_rules! log_debug {
	($logger: expr, $($arg:tt)*) => (
		log_given_level!($logger, lightning::util::logger::Level::Debug, $($arg)*)
	)
}

#[allow(unused_macros)]
macro_rules! log_trace {
	($logger: expr, $($arg:tt)*) => (
		log_given_level!($logger, lightning::util::logger::Level::Trace, $($arg)*)
	)
}
pub(crate) use log_trace;
