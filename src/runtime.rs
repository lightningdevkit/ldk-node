// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightning::util::native_async::FutureSpawner;
use tokio::task::{JoinHandle, JoinSet};

use crate::config::{
	BACKGROUND_TASK_SHUTDOWN_TIMEOUT_SECS, LDK_EVENT_HANDLER_SHUTDOWN_TIMEOUT_SECS,
};
use crate::logger::{log_debug, log_error, log_trace, LdkLogger, Logger};

pub(crate) struct Runtime {
	mode: RuntimeMode,
	background_tasks: Mutex<JoinSet<()>>,
	cancellable_background_tasks: Mutex<CancellableBackgroundTasks>,
	background_processor_task: Mutex<Option<JoinHandle<()>>>,
	logger: Arc<Logger>,
}

struct CancellableBackgroundTasks {
	tasks: JoinSet<()>,
	accepting_tasks: bool,
}

impl CancellableBackgroundTasks {
	fn new() -> Self {
		Self { tasks: JoinSet::new(), accepting_tasks: true }
	}
}

impl Runtime {
	pub fn new(logger: Arc<Logger>) -> Result<Self, std::io::Error> {
		let mode = match tokio::runtime::Handle::try_current() {
			Ok(handle) => RuntimeMode::Handle(handle),
			Err(_) => {
				let mut runtime_builder = tokio::runtime::Builder::new_multi_thread();
				runtime_builder.enable_all();
				runtime_builder.thread_name_fn(|| {
					static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
					let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
					format!("ldk-node-runtime-{}", id)
				});
				// Eager driver handoff lets Tokio move the I/O driver to another worker sooner
				// when this runtime's current worker enters `block_in_place` via `block_on`.
				// That marginally reduces the chance that a synchronous caller blocks the same
				// worker that would otherwise drive the I/O resource it is waiting on. It does
				// not solve the issue completely: it only applies to node runtimes we build
				// ourselves under `tokio_unstable`, does not affect externally supplied runtime
				// handles, and cannot guarantee that every persistence driver task needed by the
				// blocked future is already polling elsewhere. See the `StoreRuntime` docs below
				// for the full deadlock scenario and the temporary store-runtime isolation.
				#[cfg(tokio_unstable)]
				runtime_builder.enable_eager_driver_handoff();
				let rt = runtime_builder.build()?;
				RuntimeMode::Owned(rt)
			},
		};
		let background_tasks = Mutex::new(JoinSet::new());
		let cancellable_background_tasks = Mutex::new(CancellableBackgroundTasks::new());
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
		let cancellable_background_tasks = Mutex::new(CancellableBackgroundTasks::new());
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
		let mut background_tasks = self.background_tasks.lock().expect("lock");
		let runtime_handle = self.handle();
		// Since it seems to make a difference to `tokio` (see
		// https://docs.rs/tokio/latest/tokio/time/fn.timeout.html#panics) we make sure the futures
		// are always put in an `async` / `.await` closure.
		background_tasks.spawn_on(async { future.await }, runtime_handle);
	}

	pub fn spawn_cancellable_background_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let mut cancellable_background_tasks =
			self.cancellable_background_tasks.lock().expect("lock");
		if !cancellable_background_tasks.accepting_tasks {
			log_trace!(
				self.logger,
				"Ignoring cancellable background task spawned during shutdown."
			);
			return;
		}
		let runtime_handle = self.handle();
		// Since it seems to make a difference to `tokio` (see
		// https://docs.rs/tokio/latest/tokio/time/fn.timeout.html#panics) we make sure the futures
		// are always put in an `async` / `.await` closure.
		cancellable_background_tasks.tasks.spawn_on(async { future.await }, runtime_handle);
	}

	pub fn allow_cancellable_background_task_spawns(&self) {
		self.cancellable_background_tasks.lock().expect("lock").accepting_tasks = true;
	}

	pub fn spawn_background_processor_task<F>(&self, future: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let mut background_processor_task = self.background_processor_task.lock().expect("lock");
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
		// Since it seems to make a difference to `tokio` (see
		// https://docs.rs/tokio/latest/tokio/time/fn.timeout.html#panics) we make sure the futures
		// are always put in an `async` / `.await` closure.
		tokio::task::block_in_place(move || handle.block_on(async { future.await }))
	}

	pub fn abort_cancellable_background_tasks(&self) {
		let mut tasks = {
			let mut cancellable_background_tasks =
				self.cancellable_background_tasks.lock().expect("lock");
			cancellable_background_tasks.accepting_tasks = false;
			core::mem::take(&mut cancellable_background_tasks.tasks)
		};
		debug_assert!(tasks.len() > 0, "Expected some cancellable background_tasks");
		tasks.abort_all();
		self.block_on(async { while let Some(_) = tasks.join_next().await {} })
	}

	pub fn wait_on_background_tasks(&self) {
		let mut tasks = core::mem::take(&mut *self.background_tasks.lock().expect("lock"));
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
			self.background_processor_task.lock().expect("lock").take()
		{
			let abort_handle = background_processor_task.abort_handle();
			// Since it seems to make a difference to `tokio` (see
			// https://docs.rs/tokio/latest/tokio/time/fn.timeout.html#panics) we make sure the futures
			// are always put in an `async` / `.await` closure.
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
			runtime_handle.metrics().num_alive_tasks()
		);
	}

	pub(crate) fn handle(&self) -> &tokio::runtime::Handle {
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

/// Runtime used by async store backends while ldk-node still exposes synchronous APIs.
///
/// This is a temporary bridge for store implementations that need Tokio-driven I/O, such as VSS
/// and PostgreSQL. Many public ldk-node methods are still synchronous, so they call
/// [`Runtime::block_on`] when they need to wait for async persistence. If that persistence work is
/// driven by the same Tokio runtime as the synchronous caller, a blocking call can deadlock in a
/// narrow but realistic scheduler state.
///
/// The failure mode is that `block_on` parks the current worker with `block_in_place` while it
/// waits for an async store operation. Suppose that store operation is waiting for an I/O future,
/// and the connection driver or I/O driver task that can make the future progress is assigned to
/// the same worker thread that just entered `block_in_place`. The blocked sync caller is waiting
/// for the persistence future to complete, while the persistence future is waiting for an I/O task
/// that cannot be polled because its worker is occupied by the blocking caller. With no worker
/// driving that I/O resource, neither side can make progress.
///
/// A simple example is a synchronous node API calling `block_on(store.write(...))` for a
/// tokio-postgres-backed store. The write future may wait for the postgres connection task or
/// socket readiness. If the runtime worker that should poll that connection task is also the
/// worker currently blocked in the synchronous API, the write cannot complete, and the synchronous
/// API cannot unblock.
///
/// `StoreRuntime` gives each such store backend its own small runtime, workers, and I/O driver.
/// Synchronous node APIs may still block the node runtime while waiting for persistence, but the
/// persistence tasks they wait on are driven independently and can continue polling sockets and
/// connection drivers.
///
/// Once ldk-node switches the remaining store-backed APIs to be fully async, callers will await
/// persistence directly and these `block_on` bridges will be disallowed. At that point the store
/// runtimes should be removed again and store I/O can run on the node runtime directly.
pub(crate) struct StoreRuntime {
	runtime: Option<tokio::runtime::Runtime>,
}

impl StoreRuntime {
	pub(crate) fn new(
		thread_name_prefix: &'static str, worker_threads: usize, runtime_name: &'static str,
	) -> io::Result<Self> {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.thread_name_fn(move || {
				static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
				let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
				format!("{}-{}", thread_name_prefix, id)
			})
			.worker_threads(worker_threads)
			.max_blocking_threads(worker_threads)
			.build()
			.map_err(|e| {
				io::Error::new(
					io::ErrorKind::Other,
					format!("Failed to build {runtime_name} runtime: {e}"),
				)
			})?;
		Ok(Self { runtime: Some(runtime) })
	}

	pub(crate) fn handle(&self) -> &tokio::runtime::Handle {
		self.runtime.as_ref().expect("store runtime must be available").handle()
	}

	pub(crate) fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
	where
		F: Future + Send + 'static,
		F::Output: Send + 'static,
	{
		self.handle().spawn(future)
	}

	pub(crate) fn shutdown_background(mut self) {
		if let Some(runtime) = self.runtime.take() {
			runtime.shutdown_background();
		}
	}
}

impl Drop for StoreRuntime {
	fn drop(&mut self) {
		if let Some(runtime) = self.runtime.take() {
			runtime.shutdown_background();
		}
	}
}

pub(crate) struct RuntimeSpawner {
	runtime: Arc<Runtime>,
}

impl RuntimeSpawner {
	pub(crate) fn new(runtime: Arc<Runtime>) -> Self {
		Self { runtime }
	}
}

impl FutureSpawner for RuntimeSpawner {
	type E = tokio::sync::oneshot::error::RecvError;
	type SpawnedFutureResult<O> = tokio::sync::oneshot::Receiver<O>;
	fn spawn<O: Send + 'static, F: Future<Output = O> + Send + 'static>(
		&self, future: F,
	) -> Self::SpawnedFutureResult<O> {
		let (result, output) = tokio::sync::oneshot::channel();
		self.runtime.spawn_cancellable_background_task(async move {
			// We don't care if the send works or not, if the receiver is dropped its not our
			// problem.
			let _ = result.send(future.await);
		});
		output
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use tokio::sync::oneshot;

	fn test_runtime() -> Runtime {
		Runtime::new(Arc::new(Logger::new_log_facade())).unwrap()
	}

	#[test]
	fn late_cancellable_spawns_are_not_polled_after_abort() {
		let runtime = test_runtime();
		let (started_sender, started_receiver) = oneshot::channel();
		runtime.spawn_cancellable_background_task(async move {
			let _ = started_sender.send(());
			std::future::pending::<()>().await;
		});
		runtime.block_on(async {
			started_receiver.await.expect("initial task should start");
		});

		runtime.abort_cancellable_background_tasks();

		let (late_spawn_sender, late_spawn_receiver) = oneshot::channel();
		runtime.spawn_cancellable_background_task(async move {
			let _ = late_spawn_sender.send(());
		});
		let late_spawn_was_polled = runtime.block_on(async {
			match tokio::time::timeout(Duration::from_secs(1), late_spawn_receiver).await {
				Ok(Ok(())) => true,
				Ok(Err(_)) | Err(_) => false,
			}
		});

		assert!(
			!late_spawn_was_polled,
			"cancellable task spawned after shutdown started should not be polled"
		);

		runtime.allow_cancellable_background_task_spawns();

		let (restarted_sender, restarted_receiver) = oneshot::channel();
		runtime.spawn_cancellable_background_task(async move {
			let _ = restarted_sender.send(());
		});
		runtime.block_on(async {
			restarted_receiver.await.expect("spawn should be allowed after restart");
		});
		runtime.abort_cancellable_background_tasks();
	}
}
