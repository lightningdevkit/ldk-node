use lightning::util::persist::KVStorePersister;
use lightning::util::ser::Writeable;

use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct TestPersister {
	pending_persist: AtomicBool,
}

impl TestPersister {
	pub fn new() -> Self {
		let pending_persist = AtomicBool::new(false);
		Self { pending_persist }
	}

	pub fn get_and_clear_pending_persist(&self) -> bool {
		self.pending_persist.swap(false, Ordering::SeqCst)
	}
}

impl KVStorePersister for TestPersister {
	fn persist<W: Writeable>(&self, _key: &str, _object: &W) -> std::io::Result<()> {
		self.pending_persist.store(true, Ordering::SeqCst);
		Ok(())
	}
}
