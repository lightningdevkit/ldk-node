// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::collections::{hash_map, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use lightning::io;
use lightning::util::persist::{
	KVStore, KVStoreSync, PageToken, PaginatedKVStore, PaginatedKVStoreSync, PaginatedListResponse,
};

const IN_MEMORY_PAGE_SIZE: usize = 50;

pub struct InMemoryStore {
	persisted_bytes: Mutex<HashMap<String, HashMap<String, Vec<u8>>>>,
	creation_counter: AtomicU64,
	creation_times: Mutex<HashMap<String, HashMap<String, u64>>>,
}

impl InMemoryStore {
	pub fn new() -> Self {
		let persisted_bytes = Mutex::new(HashMap::new());
		let creation_counter = AtomicU64::new(1);
		let creation_times = Mutex::new(HashMap::new());
		Self { persisted_bytes, creation_counter, creation_times }
	}

	fn read_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		let persisted_lock = self.persisted_bytes.lock().unwrap();
		let prefixed = format!("{primary_namespace}/{secondary_namespace}");

		if let Some(outer_ref) = persisted_lock.get(&prefixed) {
			if let Some(inner_ref) = outer_ref.get(key) {
				let bytes = inner_ref.clone();
				Ok(bytes)
			} else {
				Err(io::Error::new(io::ErrorKind::NotFound, "Key not found"))
			}
		} else {
			Err(io::Error::new(io::ErrorKind::NotFound, "Namespace not found"))
		}
	}

	fn write_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();

		let prefixed = format!("{primary_namespace}/{secondary_namespace}");
		let outer_e = persisted_lock.entry(prefixed.clone()).or_insert(HashMap::new());
		outer_e.insert(key.to_string(), buf);

		// Only assign creation time on first write (not on update)
		let mut ct_lock = self.creation_times.lock().unwrap();
		let ct_ns = ct_lock.entry(prefixed).or_insert(HashMap::new());
		ct_ns
			.entry(key.to_string())
			.or_insert_with(|| self.creation_counter.fetch_add(1, Ordering::Relaxed));

		Ok(())
	}

	fn remove_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, _lazy: bool,
	) -> io::Result<()> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();

		let prefixed = format!("{primary_namespace}/{secondary_namespace}");
		if let Some(outer_ref) = persisted_lock.get_mut(&prefixed) {
			outer_ref.remove(&key.to_string());
		}

		// Remove creation time entry
		let mut ct_lock = self.creation_times.lock().unwrap();
		if let Some(ct_ns) = ct_lock.get_mut(&prefixed) {
			ct_ns.remove(key);
		}

		Ok(())
	}

	fn list_internal(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> io::Result<Vec<String>> {
		let mut persisted_lock = self.persisted_bytes.lock().unwrap();

		let prefixed = format!("{primary_namespace}/{secondary_namespace}");
		match persisted_lock.entry(prefixed) {
			hash_map::Entry::Occupied(e) => Ok(e.get().keys().cloned().collect()),
			hash_map::Entry::Vacant(_) => Ok(Vec::new()),
		}
	}
}

impl KVStore for InMemoryStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> impl Future<Output = Result<Vec<u8>, io::Error>> + 'static + Send {
		let res = self.read_internal(&primary_namespace, &secondary_namespace, &key);
		async move { res }
	}
	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let res = self.write_internal(&primary_namespace, &secondary_namespace, &key, buf);
		async move { res }
	}
	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> impl Future<Output = Result<(), io::Error>> + 'static + Send {
		let res = self.remove_internal(&primary_namespace, &secondary_namespace, &key, lazy);
		async move { res }
	}
	fn list(
		&self, primary_namespace: &str, secondary_namespace: &str,
	) -> impl Future<Output = Result<Vec<String>, io::Error>> + 'static + Send {
		let res = self.list_internal(primary_namespace, secondary_namespace);
		async move { res }
	}
}

impl KVStoreSync for InMemoryStore {
	fn read(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str,
	) -> io::Result<Vec<u8>> {
		self.read_internal(primary_namespace, secondary_namespace, key)
	}

	fn write(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, buf: Vec<u8>,
	) -> io::Result<()> {
		self.write_internal(primary_namespace, secondary_namespace, key, buf)
	}

	fn remove(
		&self, primary_namespace: &str, secondary_namespace: &str, key: &str, lazy: bool,
	) -> io::Result<()> {
		self.remove_internal(primary_namespace, secondary_namespace, key, lazy)
	}

	fn list(&self, primary_namespace: &str, secondary_namespace: &str) -> io::Result<Vec<String>> {
		self.list_internal(primary_namespace, secondary_namespace)
	}
}

impl InMemoryStore {
	fn list_paginated_internal(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> io::Result<PaginatedListResponse> {
		let ct_lock = self.creation_times.lock().unwrap();
		let prefixed = format!("{primary_namespace}/{secondary_namespace}");

		let ct_ns = match ct_lock.get(&prefixed) {
			Some(m) => m,
			None => {
				return Ok(PaginatedListResponse { keys: Vec::new(), next_page_token: None });
			},
		};

		// Build list of (key, sort_order) sorted by sort_order DESC (newest first).
		let mut entries: Vec<(&String, &u64)> = ct_ns.iter().collect();
		entries.sort_by(|a, b| b.1.cmp(a.1));

		// Apply page token filter
		let start_idx = if let Some(ref token) = page_token {
			let token_sort_order: u64 = token
				.as_str()
				.parse()
				.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid page token"))?;

			entries
				.iter()
				.position(|(_, sort_order)| **sort_order < token_sort_order)
				.unwrap_or(entries.len())
		} else {
			0
		};

		// Fetch one extra entry beyond page size to determine whether a next page exists.
		let mut page: Vec<(&String, &u64)> =
			entries[start_idx..].iter().take(IN_MEMORY_PAGE_SIZE + 1).cloned().collect();

		let has_more = page.len() > IN_MEMORY_PAGE_SIZE;
		page.truncate(IN_MEMORY_PAGE_SIZE);

		let next_page_token = if has_more {
			let (_, last_sort_order) = page.last().unwrap();
			Some(PageToken::new(last_sort_order.to_string()))
		} else {
			None
		};

		let page: Vec<String> = page.into_iter().map(|(k, _)| k.clone()).collect();

		Ok(PaginatedListResponse { keys: page, next_page_token })
	}
}

impl PaginatedKVStoreSync for InMemoryStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> io::Result<PaginatedListResponse> {
		self.list_paginated_internal(primary_namespace, secondary_namespace, page_token)
	}
}

impl PaginatedKVStore for InMemoryStore {
	fn list_paginated(
		&self, primary_namespace: &str, secondary_namespace: &str, page_token: Option<PageToken>,
	) -> impl Future<Output = Result<PaginatedListResponse, io::Error>> + 'static + Send {
		let res = self.list_paginated_internal(primary_namespace, secondary_namespace, page_token);
		async move { res }
	}
}

unsafe impl Sync for InMemoryStore {}
unsafe impl Send for InMemoryStore {}
