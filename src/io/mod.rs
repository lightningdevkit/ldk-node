pub(crate) mod utils;

use lightning_persister::FilesystemPersister;

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

/// Provides an interface that allows a previously persisted key to be unpersisted.
pub trait KVStoreUnpersister {
	/// Unpersist (i.e., remove) the writeable previously persisted under the provided key.
	/// Returns `true` if the key was present, and `false` otherwise.
	fn unpersist(&self, key: &str) -> std::io::Result<bool>;
}

impl KVStoreUnpersister for FilesystemPersister {
	fn unpersist(&self, key: &str) -> std::io::Result<bool> {
		let mut dest_file = PathBuf::from(self.get_data_dir());
		dest_file.push(key);

		if !dest_file.is_file() {
			return Ok(false);
		}

		fs::remove_file(&dest_file)?;
		#[cfg(not(target_os = "windows"))]
		{
			let parent_directory = dest_file.parent().unwrap();
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

		if dest_file.is_file() {
			return Err(std::io::Error::new(std::io::ErrorKind::Other, "Unpersisting key failed"));
		}

		return Ok(true);
	}
}
