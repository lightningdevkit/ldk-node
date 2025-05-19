// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use tokio::task::JoinHandle;

use std::future::Future;

pub(crate) struct Runtime {
	mode: RuntimeMode,
}

impl Runtime {
	pub fn new() -> Result<Self, std::io::Error> {
		let mode = match tokio::runtime::Handle::try_current() {
			Ok(handle) => RuntimeMode::Handle(handle),
			Err(_) => {
				let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
				RuntimeMode::Owned(rt)
			},
		};
		Ok(Self { mode })
	}

	pub fn with_handle(handle: tokio::runtime::Handle) -> Self {
		let mode = RuntimeMode::Handle(handle);
		Self { mode }
	}

	pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
	where
		F: Future + Send + 'static,
		F::Output: Send + 'static,
	{
		let handle = self.handle();
		handle.spawn(future)
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

	pub fn handle(&self) -> &tokio::runtime::Handle {
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
