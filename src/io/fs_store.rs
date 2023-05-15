#[cfg(target_os = "windows")]
extern crate winapi;

use super::KVStore;

use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};

#[cfg(not(target_os = "windows"))]
use std::os::unix::io::AsRawFd;

use lightning::util::persist::KVStorePersister;
use lightning::util::ser::Writeable;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

#[cfg(target_os = "windows")]
use {std::ffi::OsStr, std::os::windows::ffi::OsStrExt};

#[cfg(target_os = "windows")]
macro_rules! call {
	($e: expr) => {
		if $e != 0 {
			return Ok(());
		} else {
			return Err(std::io::Error::last_os_error());
		}
	};
}

#[cfg(target_os = "windows")]
fn path_to_windows_str<T: AsRef<OsStr>>(path: T) -> Vec<winapi::shared::ntdef::WCHAR> {
	path.as_ref().encode_wide().chain(Some(0)).collect()
}

pub struct FilesystemStore {
	dest_dir: PathBuf,
	locks: Mutex<HashMap<(String, String), Arc<RwLock<()>>>>,
}

impl FilesystemStore {
	pub fn new(dest_dir: PathBuf) -> Self {
		let locks = Mutex::new(HashMap::new());
		Self { dest_dir, locks }
	}
}

impl KVStore for FilesystemStore {
	type Reader = FilesystemReader;

	fn read(&self, namespace: &str, key: &str) -> std::io::Result<Self::Reader> {
		let mut outer_lock = self.locks.lock().unwrap();
		let lock_key = (namespace.to_string(), key.to_string());
		let inner_lock_ref = Arc::clone(&outer_lock.entry(lock_key).or_default());

		let mut dest_file_path = self.dest_dir.clone();
		dest_file_path.push(namespace);
		dest_file_path.push(key);
		FilesystemReader::new(dest_file_path, inner_lock_ref)
	}

	fn write(&self, namespace: &str, key: &str, buf: &[u8]) -> std::io::Result<()> {
		let mut outer_lock = self.locks.lock().unwrap();
		let lock_key = (namespace.to_string(), key.to_string());
		let inner_lock_ref = Arc::clone(&outer_lock.entry(lock_key).or_default());
		let _guard = inner_lock_ref.write().unwrap();

		let mut dest_file_path = self.dest_dir.clone();
		dest_file_path.push(namespace);
		dest_file_path.push(key);

		let msg = format!("Could not retrieve parent directory of {}.", dest_file_path.display());
		let parent_directory = dest_file_path
			.parent()
			.ok_or(std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))?
			.to_path_buf();
		fs::create_dir_all(parent_directory.clone())?;

		// Do a crazy dance with lots of fsync()s to be overly cautious here...
		// We never want to end up in a state where we've lost the old data, or end up using the
		// old data on power loss after we've returned.
		// The way to atomically write a file on Unix platforms is:
		// open(tmpname), write(tmpfile), fsync(tmpfile), close(tmpfile), rename(), fsync(dir)
		let mut tmp_file_path = dest_file_path.clone();
		let mut rng = thread_rng();
		let rand_str: String = (0..7).map(|_| rng.sample(Alphanumeric) as char).collect();
		let ext = format!("{}.tmp", rand_str);
		tmp_file_path.set_extension(ext);

		let mut tmp_file = fs::File::create(&tmp_file_path)?;
		tmp_file.write_all(&buf)?;
		tmp_file.sync_all()?;

		#[cfg(not(target_os = "windows"))]
		{
			fs::rename(&tmp_file_path, &dest_file_path)?;
			let dir_file = fs::OpenOptions::new().read(true).open(parent_directory.clone())?;
			unsafe {
				libc::fsync(dir_file.as_raw_fd());
			}
		}

		#[cfg(target_os = "windows")]
		{
			if dest_file_path.exists() {
				unsafe {
					winapi::um::winbase::ReplaceFileW(
						path_to_windows_str(dest_file_path).as_ptr(),
						path_to_windows_str(tmp_file_path).as_ptr(),
						std::ptr::null(),
						winapi::um::winbase::REPLACEFILE_IGNORE_MERGE_ERRORS,
						std::ptr::null_mut() as *mut winapi::ctypes::c_void,
						std::ptr::null_mut() as *mut winapi::ctypes::c_void,
					)
				};
			} else {
				call!(unsafe {
					winapi::um::winbase::MoveFileExW(
						path_to_windows_str(tmp_file_path).as_ptr(),
						path_to_windows_str(dest_file_path).as_ptr(),
						winapi::um::winbase::MOVEFILE_WRITE_THROUGH
							| winapi::um::winbase::MOVEFILE_REPLACE_EXISTING,
					)
				});
			}
		}
		Ok(())
	}

	fn remove(&self, namespace: &str, key: &str) -> std::io::Result<bool> {
		let mut outer_lock = self.locks.lock().unwrap();
		let lock_key = (namespace.to_string(), key.to_string());
		let inner_lock_ref = Arc::clone(&outer_lock.entry(lock_key.clone()).or_default());

		let _guard = inner_lock_ref.write().unwrap();

		let mut dest_file_path = self.dest_dir.clone();
		dest_file_path.push(namespace);
		dest_file_path.push(key);

		if !dest_file_path.is_file() {
			return Ok(false);
		}

		fs::remove_file(&dest_file_path)?;
		#[cfg(not(target_os = "windows"))]
		{
			let msg =
				format!("Could not retrieve parent directory of {}.", dest_file_path.display());
			let parent_directory = dest_file_path
				.parent()
				.ok_or(std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))?;
			let dir_file = fs::OpenOptions::new().read(true).open(parent_directory)?;
			unsafe {
				// The above call to `fs::remove_file` corresponds to POSIX `unlink`, whose changes
				// to the inode might get cached (and hence possibly lost on crash), depending on
				// the target platform and file system.
				//
				// In order to assert we permanently removed the file in question we therefore
				// call `fsync` on the parent directory on platforms that support it,
				libc::fsync(dir_file.as_raw_fd());
			}
		}

		if dest_file_path.is_file() {
			return Err(std::io::Error::new(std::io::ErrorKind::Other, "Removing key failed"));
		}

		if Arc::strong_count(&inner_lock_ref) == 2 {
			// It's safe to remove the lock entry if we're the only one left holding a strong
			// reference. Checking this is necessary to ensure we continue to distribute references to the
			// same lock as long as some Readers are around. However, we still want to
			// clean up the table when possible.
			//
			// Note that this by itself is still leaky as lock entries will remain when more Readers/Writers are
			// around, but is preferable to doing nothing *or* something overly complex such as
			// implementing yet another RAII structure just for this pupose.
			outer_lock.remove(&lock_key);
		}

		// Garbage collect all lock entries that are not referenced anymore.
		outer_lock.retain(|_, v| Arc::strong_count(&v) > 1);

		Ok(true)
	}

	fn list(&self, namespace: &str) -> std::io::Result<Vec<String>> {
		let mut prefixed_dest = self.dest_dir.clone();
		prefixed_dest.push(namespace);

		let mut keys = Vec::new();

		if !Path::new(&prefixed_dest).exists() {
			return Ok(Vec::new());
		}

		for entry in fs::read_dir(prefixed_dest.clone())? {
			let entry = entry?;
			let p = entry.path();

			if !p.is_file() {
				continue;
			}

			if let Some(ext) = p.extension() {
				if ext == "tmp" {
					continue;
				}
			}

			if let Ok(relative_path) = p.strip_prefix(prefixed_dest.clone()) {
				keys.push(relative_path.display().to_string())
			}
		}

		Ok(keys)
	}
}

pub struct FilesystemReader {
	inner: BufReader<fs::File>,
	lock_ref: Arc<RwLock<()>>,
}

impl FilesystemReader {
	fn new(dest_file_path: PathBuf, lock_ref: Arc<RwLock<()>>) -> std::io::Result<Self> {
		let f = fs::File::open(dest_file_path.clone())?;
		let inner = BufReader::new(f);
		Ok(Self { inner, lock_ref })
	}
}

impl Read for FilesystemReader {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		let _guard = self.lock_ref.read().unwrap();
		self.inner.read(buf)
	}
}

impl KVStorePersister for FilesystemStore {
	fn persist<W: Writeable>(&self, prefixed_key: &str, object: &W) -> lightning::io::Result<()> {
		let msg = format!("Could not persist file for key {}.", prefixed_key);
		let dest_file_path = PathBuf::from_str(prefixed_key).map_err(|_| {
			lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg.clone())
		})?;

		let parent_directory = dest_file_path.parent().ok_or(lightning::io::Error::new(
			lightning::io::ErrorKind::InvalidInput,
			msg.clone(),
		))?;
		let namespace = parent_directory.display().to_string();

		let dest_without_namespace = dest_file_path
			.strip_prefix(&namespace)
			.map_err(|_| lightning::io::Error::new(lightning::io::ErrorKind::InvalidInput, msg))?;
		let key = dest_without_namespace.display().to_string();

		self.write(&namespace, &key, &object.encode())?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test::utils::random_storage_path;
	use lightning::util::persist::KVStorePersister;
	use lightning::util::ser::Readable;

	use proptest::prelude::*;
	proptest! {
		#[test]
		fn read_write_remove_list_persist(data in any::<[u8; 32]>()) {
			let rand_dir = random_storage_path();

			let fs_store = FilesystemStore::new(rand_dir.into());
			let namespace = "testspace";
			let key = "testkey";

			// Test the basic KVStore operations.
			fs_store.write(namespace, key, &data).unwrap();

			let listed_keys = fs_store.list(namespace).unwrap();
			assert_eq!(listed_keys.len(), 1);
			assert_eq!(listed_keys[0], "testkey");

			let mut reader = fs_store.read(namespace, key).unwrap();
			let read_data: [u8; 32] = Readable::read(&mut reader).unwrap();
			assert_eq!(data, read_data);

			fs_store.remove(namespace, key).unwrap();

			let listed_keys = fs_store.list(namespace).unwrap();
			assert_eq!(listed_keys.len(), 0);

			// Test KVStorePersister
			let prefixed_key = format!("{}/{}", namespace, key);
			fs_store.persist(&prefixed_key, &data).unwrap();
			let mut reader = fs_store.read(namespace, key).unwrap();
			let read_data: [u8; 32] = Readable::read(&mut reader).unwrap();
			assert_eq!(data, read_data);
		}
	}
}
