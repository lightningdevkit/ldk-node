// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(crate) trait MutexExt<T> {
	fn lck(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
	fn lck(&self) -> MutexGuard<'_, T> {
		self.lock().expect("mutex poisoning indicates a broken internal invariant")
	}
}

pub(crate) trait RwLockExt<T> {
	fn rlck(&self) -> RwLockReadGuard<'_, T>;
	fn wlck(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
	fn rlck(&self) -> RwLockReadGuard<'_, T> {
		self.read().expect("rwlock poisoning indicates a broken internal invariant")
	}

	fn wlck(&self) -> RwLockWriteGuard<'_, T> {
		self.write().expect("rwlock poisoning indicates a broken internal invariant")
	}
}
