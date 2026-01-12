use std::sync::{Arc, Mutex};

use chrono::Utc;
use ldk_node::logger::{LogContext, LogLevel, LogRecord, LogWriter};
#[cfg(not(feature = "uniffi"))]
use log::Record as LogFacadeRecord;
use log::{Level as LogFacadeLevel, LevelFilter as LogFacadeLevelFilter, Log as LogFacadeLog};

#[derive(Clone)]
pub(crate) enum TestLogWriter {
	FileWriter,
	LogFacade,
	Custom(Arc<dyn LogWriter>),
}

impl Default for TestLogWriter {
	fn default() -> Self {
		TestLogWriter::FileWriter
	}
}

pub(crate) struct MockLogFacadeLogger {
	logs: Arc<Mutex<Vec<String>>>,
}

impl MockLogFacadeLogger {
	pub fn new() -> Self {
		Self { logs: Arc::new(Mutex::new(Vec::new())) }
	}

	pub fn retrieve_logs(&self) -> Vec<String> {
		self.logs.lock().unwrap().to_vec()
	}
}

impl LogFacadeLog for MockLogFacadeLogger {
	fn enabled(&self, _metadata: &log::Metadata) -> bool {
		true
	}

	fn log(&self, record: &log::Record) {
		let message = format!(
			"{} {:<5} [{}:{}] {}",
			Utc::now().format("%Y-%m-%d %H:%M:%S"),
			record.level().to_string(),
			record.module_path().unwrap(),
			record.line().unwrap(),
			record.args()
		);
		self.logs.lock().unwrap().push(message);
	}

	fn flush(&self) {}
}

#[cfg(not(feature = "uniffi"))]
impl LogWriter for MockLogFacadeLogger {
	fn log<'a>(&self, record: LogRecord) {
		let record = MockLogRecord(record).into();
		LogFacadeLog::log(self, &record);
	}
}

#[cfg(not(feature = "uniffi"))]
struct MockLogRecord<'a>(LogRecord<'a>);
struct MockLogLevel(LogLevel);

impl From<MockLogLevel> for LogFacadeLevel {
	fn from(level: MockLogLevel) -> Self {
		match level.0 {
			LogLevel::Gossip | LogLevel::Trace => LogFacadeLevel::Trace,
			LogLevel::Debug => LogFacadeLevel::Debug,
			LogLevel::Info => LogFacadeLevel::Info,
			LogLevel::Warn => LogFacadeLevel::Warn,
			LogLevel::Error => LogFacadeLevel::Error,
		}
	}
}

#[cfg(not(feature = "uniffi"))]
impl<'a> From<MockLogRecord<'a>> for LogFacadeRecord<'a> {
	fn from(log_record: MockLogRecord<'a>) -> Self {
		let log_record = log_record.0;
		let level = MockLogLevel(log_record.level).into();

		let mut record_builder = LogFacadeRecord::builder();
		let record = record_builder
			.level(level)
			.module_path(Some(&log_record.module_path))
			.line(Some(log_record.line))
			.args(log_record.args);

		record.build()
	}
}

pub(crate) fn init_log_logger(level: LogFacadeLevelFilter) -> Arc<MockLogFacadeLogger> {
	let logger = Arc::new(MockLogFacadeLogger::new());
	log::set_boxed_logger(Box::new(logger.clone())).unwrap();
	log::set_max_level(level);

	logger
}

pub(crate) fn validate_log_entry(entry: &String) {
	let parts = entry.splitn(4, ' ').collect::<Vec<_>>();
	assert_eq!(parts.len(), 4);
	let (day, time, level, path_and_msg) = (parts[0], parts[1], parts[2], parts[3]);

	let day_parts = day.split('-').collect::<Vec<_>>();
	assert_eq!(day_parts.len(), 3);
	let (year, month, day) = (day_parts[0], day_parts[1], day_parts[2]);
	assert!(year.len() == 4 && month.len() == 2 && day.len() == 2);
	assert!(
		year.chars().all(|c| c.is_digit(10))
			&& month.chars().all(|c| c.is_digit(10))
			&& day.chars().all(|c| c.is_digit(10))
	);

	let time_parts = time.split(':').collect::<Vec<_>>();
	assert_eq!(time_parts.len(), 3);
	let (hour, minute, second) = (time_parts[0], time_parts[1], time_parts[2]);
	assert!(hour.len() == 2 && minute.len() == 2 && second.len() == 2);
	assert!(
		hour.chars().all(|c| c.is_digit(10))
			&& minute.chars().all(|c| c.is_digit(10))
			&& second.chars().all(|c| c.is_digit(10))
	);

	assert!(["GOSSIP", "TRACE", "DEBUG", "INFO", "WARN", "ERROR"].contains(&level),);

	let path = path_and_msg.split_whitespace().next().unwrap();
	assert!(path.contains('[') && path.contains(']'));
	let module_path = &path[1..path.len() - 1];
	let path_parts = module_path.rsplitn(2, ':').collect::<Vec<_>>();
	assert_eq!(path_parts.len(), 2);
	let (line_number, module_name) = (path_parts[0], path_parts[1]);
	assert!(module_name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ':'));
	assert!(line_number.chars().all(|c| c.is_digit(10)));

	let msg_start_index = path_and_msg.find(']').unwrap() + 1;
	let msg = &path_and_msg[msg_start_index..];
	assert!(!msg.is_empty());
}

pub(crate) struct MultiNodeLogger {
	node_id: String,
}

impl MultiNodeLogger {
	pub(crate) fn new(node_id: String) -> Self {
		Self { node_id }
	}
}

impl LogWriter for MultiNodeLogger {
	fn log(&self, record: LogRecord) {
		let log = format!(
			"[{}] {} {:<5} [{}:{}] {}{}\n",
			self.node_id,
			Utc::now().format("%Y-%m-%d %H:%M:%S%.3f"),
			record.level.to_string(),
			record.module_path,
			record.line,
			record.args,
			LogContext {
				channel_id: record.channel_id.as_ref(),
				peer_id: record.peer_id.as_ref(),
				payment_hash: record.payment_hash.as_ref(),
			},
		);

		print!("{}", log);
	}
}
