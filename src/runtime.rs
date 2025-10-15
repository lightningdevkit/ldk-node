// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::{JoinHandle, JoinSet};

use crate::config::{
	BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS, LDK_EVENT_HANDLER_SHUTDOWN_TIMEOUT_SECS,
};
use crate::logger::{log_debug, log_error, log_trace, LdkLogger, Logger};

pub(crate) struct Runtime {
	mode: RuntimeMode,
	background_tasks: Mutex<JoinSet<()>>,
	cancellable_background_tasks: Mutex<JoinSet<()>>,
	background_processor_task: Mutex<Option<JoinHandle<()>>>,
	logger: Arc<Logger>,
}

impl Runtime {
	pub fn new(logger: Arc<Logger>) -> Result<Self, std::io::Error> {
		let mode = match tokio::runtime::Handle::try_current() {
			Ok(handle) => RuntimeMode::Handle(handle),
			Err(_) => {
				let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
				RuntimeMode::Owned(rt)
			},
		};
		let background_tasks = Mutex::new(JoinSet::new());
		let cancellable_background_tasks = Mutex::new(JoinSet::new());
		let background_processor_task = Mutex::new(None);

		Ok(Self {
			mode,
			background_tasks,
			cancellable_background_tasks,
			background_processor_task,
			logger,
		})
	}

	pub fn with_handle(handle: tokio::runtime::Handle, logger: Arc<Logger>) -> Self {
		let mode = RuntimeMode::Handle(handle);
		let background_tasks = Mutex::new(JoinSet::new());
		let cancellable_background_tasks = Mutex::new(JoinSet::new());
		let background_processor_task = Mutex::new(None);

		Self {
			mode,
			background_tasks,
			cancellable_background_tasks,
			background_processor_task,
			logger,
		}
	}

	pub fn spawn_background_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let mut background_tasks = self.background_tasks.lock().unwrap();
		let runtime_handle = self.handle();
		background_tasks.spawn_on(future, runtime_handle);
	}

	pub fn spawn_cancellable_background_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let mut cancellable_background_tasks = self.cancellable_background_tasks.lock().unwrap();
		let runtime_handle = self.handle();
		cancellable_background_tasks.spawn_on(future, runtime_handle);
	}

	pub fn spawn_background_processor_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let mut background_processor_task = self.background_processor_task.lock().unwrap();
		debug_assert!(background_processor_task.is_none(), "Expected no background processor_task");

		let runtime_handle = self.handle();
		let handle = runtime_handle.spawn(future);
		*background_processor_task = Some(handle);
	}

	pub fn spawn_blocking<F, R>(&self, func: F) -> JoinHandle<R>
	where
		F: FnOnce() -> R + Send + 'static,
		R: Send + 'static,
	{
		let handle = self.handle();
		handle.spawn_blocking(func)
	}

	pub fn block_on<F: Future>(&self, future: F) -> F::Output {
		// While we generally decided not to overthink via which call graph users would enter our
		// runtime context, we'd still try to reuse whatever current context would be present
		// during `block_on`, as this is the context `block_in_place` would operate on. So we try
		// to detect the outer context here, and otherwise use whatever was set during
		// initialization.
		let handle = tokio::runtime::Handle::try_current().unwrap_or(self.handle().clone());
		tokio::task::block_in_place(move || handle.block_on(future))
	}

	pub fn abort_cancellable_background_tasks(&self) {
		let mut tasks = core::mem::take(&mut *self.cancellable_background_tasks.lock().unwrap());
		debug_assert!(tasks.len() > 0, "Expected some cancellable background_tasks");
		tasks.abort_all();
		self.block_on(async { while let Some(_) = tasks.join_next().await {} })
	}

	pub fn wait_on_background_tasks(&self) {
		let mut tasks = core::mem::take(&mut *self.background_tasks.lock().unwrap());
		debug_assert!(tasks.len() > 0, "Expected some background_tasks");
		self.block_on(async {
			loop {
				let timeout_fut = tokio::time::timeout(
					Duration::from_secs(BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS),
					tasks.join_next_with_id(),
				);
				match timeout_fut.await {
					Ok(Some(Ok((id, _)))) => {
						log_trace!(self.logger, "Stopped background task with id {}", id);
					},
					Ok(Some(Err(e))) => {
						tasks.abort_all();
						log_trace!(self.logger, "Stopping background task failed: {}", e);
						break;
					},
					Ok(None) => {
						log_debug!(self.logger, "Stopped all background tasks");
						break;
					},
					Err(e) => {
						tasks.abort_all();
						log_error!(self.logger, "Stopping background task timed out: {}", e);
						break;
					},
				}
			}
		})
	}

	pub fn wait_on_background_processor_task(&self) {
		if let Some(background_processor_task) =
			self.background_processor_task.lock().unwrap().take()
		{
			let abort_handle = background_processor_task.abort_handle();
			let timeout_res = self.block_on(async {
				tokio::time::timeout(
					Duration::from_secs(LDK_EVENT_HANDLER_SHUTDOWN_TIMEOUT_SECS),
					background_processor_task,
				)
				.await
			});

			match timeout_res {
				Ok(stop_res) => match stop_res {
					Ok(()) => log_debug!(self.logger, "Stopped background processing of events."),
					Err(e) => {
						abort_handle.abort();
						log_error!(
							self.logger,
							"Stopping event handling failed. This should never happen: {}",
							e
						);
						panic!("Stopping event handling failed. This should never happen.");
					},
				},
				Err(e) => {
					abort_handle.abort();
					log_error!(self.logger, "Stopping event handling timed out: {}", e);
				},
			}
		} else {
			debug_assert!(false, "Expected a background processing task");
		};
	}

	#[cfg(tokio_unstable)]
	pub fn log_metrics(&self) {
		let runtime_handle = self.handle();
		log_trace!(
			self.logger,
			"Active runtime tasks left prior to shutdown: {}",
			runtime_handle.metrics().active_tasks_count()
		);
	}

	fn handle(&self) -> &tokio::runtime::Handle {
		match &self.mode {
			RuntimeMode::Owned(rt) => rt.handle(),
			RuntimeMode::Handle(handle) => handle,
		}
	}
}

enum RuntimeMode {
	Owned(tokio::runtime::Runtime),
	Handle(tokio::runtime::Handle),
}
