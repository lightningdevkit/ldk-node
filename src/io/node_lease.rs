// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lightning::io;

pub(crate) const NODE_LEASE_DURATION: Duration = Duration::from_secs(30);
// Fail closed before the database lease expires, leaving time for process termination.
pub(crate) const NODE_LEASE_RENEWAL_DEADLINE: Duration = Duration::from_secs(20);
pub(crate) const NODE_LEASE_RENEWAL_INTERVAL: Duration = Duration::from_secs(10);
pub(crate) const NODE_LEASE_RETRY_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const NODE_LEASE_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

type LeaseLossHandler = Box<dyn FnOnce() + Send>;

pub(crate) struct NodeLease {
	owner_id: [u8; 32],
	lease_lost: AtomicBool,
	last_confirmed_renewal: Mutex<Instant>,
	loss_sender: tokio::sync::watch::Sender<bool>,
	loss_handler: Mutex<Option<LeaseLossHandler>>,
}

impl NodeLease {
	pub(crate) fn new() -> io::Result<Arc<Self>> {
		let mut owner_id = [0u8; 32];
		getrandom::fill(&mut owner_id).map_err(|e| {
			io::Error::new(io::ErrorKind::Other, format!("Failed to generate lease owner ID: {e}"))
		})?;
		let (loss_sender, _) = tokio::sync::watch::channel(false);
		Ok(Arc::new(Self {
			owner_id,
			lease_lost: AtomicBool::new(false),
			last_confirmed_renewal: Mutex::new(Instant::now()),
			loss_sender,
			loss_handler: Mutex::new(None),
		}))
	}

	pub(crate) fn owner_id(&self) -> &[u8; 32] {
		&self.owner_id
	}

	pub(crate) fn is_lost(&self) -> bool {
		self.lease_lost.load(Ordering::Acquire)
	}

	pub(crate) fn record_renewal(&self) {
		if !self.is_lost() {
			*self.last_confirmed_renewal.lock().expect("lock") = Instant::now();
		}
	}

	pub(crate) fn renewal_deadline_elapsed(&self) -> bool {
		self.last_confirmed_renewal.lock().expect("lock").elapsed() >= NODE_LEASE_RENEWAL_DEADLINE
	}

	pub(crate) fn ensure_operation_active(&self) -> io::Result<()> {
		if self.is_lost() || self.renewal_deadline_elapsed() {
			self.mark_lost();
			Err(lease_lost_error())
		} else {
			Ok(())
		}
	}

	pub(crate) fn map_operation_error(&self, error: io::Error) -> io::Error {
		// Preserve transient database errors until they outlive the local safety margin.
		self.ensure_operation_active().err().unwrap_or(error)
	}

	pub(crate) fn mark_lost(&self) {
		if self
			.lease_lost
			.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
			.is_err()
		{
			return;
		}

		// Run any installed containment handler before publishing lease loss.
		if let Some(handler) = self.loss_handler.lock().expect("lock").take() {
			handler();
		}
		self.loss_sender.send_replace(true);
	}

	pub(crate) fn set_loss_handler(&self, handler: LeaseLossHandler) {
		let mut locked_handler = self.loss_handler.lock().expect("lock");
		if self.is_lost() {
			drop(locked_handler);
			handler();
		} else {
			*locked_handler = Some(handler);
		}
	}

	pub(crate) async fn wait_for_loss(self: Arc<Self>) {
		let mut receiver = self.loss_sender.subscribe();
		let _ = receiver.wait_for(|lost| *lost).await;
	}
}

pub(crate) fn lease_lost_error() -> io::Error {
	io::Error::new(io::ErrorKind::PermissionDenied, "PostgreSQL node lease was lost")
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicBool, Ordering};

	use super::*;

	#[test]
	fn expired_operation_marks_loss_before_returning_error() {
		let lease = NodeLease::new().unwrap();
		let handler_ran = Arc::new(AtomicBool::new(false));
		let handler_ran_ref = Arc::clone(&handler_ran);
		lease.set_loss_handler(Box::new(move || {
			handler_ran_ref.store(true, Ordering::Release);
		}));
		*lease.last_confirmed_renewal.lock().unwrap() =
			Instant::now() - NODE_LEASE_RENEWAL_DEADLINE;

		let error = lease.map_operation_error(io::Error::from(io::ErrorKind::Other));

		assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
		assert!(lease.is_lost());
		assert!(handler_ran.load(Ordering::Acquire));
	}
}
