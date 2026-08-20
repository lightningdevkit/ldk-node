// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::fs;
use std::path::{Path, PathBuf};

use lightning::util::persist::migrate_kv_store_data_async;
use lightning_persister::fs_store::v1::FilesystemStore;
use lightning_persister::fs_store::v2::{FilesystemStoreV2, FilesystemStoreV2Error};

use crate::BuildError;

/// Opens a [`FilesystemStoreV2`], automatically migrating from v1 format if necessary.
///
/// If the directory contains v1 data (files at the top level), the data is migrated to v2 format
/// in a temporary directory, the original is renamed to `fs_store_v1_backup`, and the migrated
/// directory is moved into place.
pub(crate) async fn open_or_migrate_fs_store(
	storage_dir_path: PathBuf,
) -> Result<FilesystemStoreV2, BuildError> {
	let parent_dir = storage_dir_path.parent().ok_or(BuildError::StoragePathAccessFailed)?;
	fs::create_dir_all(parent_dir).map_err(|_| BuildError::StoragePathAccessFailed)?;
	recover_incomplete_fs_store_migration(&storage_dir_path)?;
	if !storage_dir_path.exists() {
		fs::create_dir_all(&storage_dir_path).map_err(|_| BuildError::StoragePathAccessFailed)?;
	}

	match FilesystemStoreV2::new(storage_dir_path.clone()) {
		Ok(store) => Ok(store),
		Err(FilesystemStoreV2Error::V1DataDetected(_)) => {
			// The directory contains v1 data, migrate to v2.
			let v1_store = FilesystemStore::new(storage_dir_path.clone());

			let v2_dir = fs_store_sibling_path(&storage_dir_path, "fs_store_v2_migrating");
			fs::create_dir_all(&v2_dir).map_err(|_| BuildError::StoragePathAccessFailed)?;
			let v2_store = FilesystemStoreV2::new(v2_dir.clone())
				.map_err(|_| BuildError::KVStoreSetupFailed)?;

			migrate_kv_store_data_async(&v1_store, &v2_store)
				.await
				.map_err(|_| BuildError::KVStoreSetupFailed)?;

			// Swap directories: rename v1 out of the way, move v2 into place.
			let backup_dir = fs_store_sibling_path(&storage_dir_path, "fs_store_v1_backup");
			fs::rename(&storage_dir_path, &backup_dir)
				.map_err(|_| BuildError::KVStoreSetupFailed)?;
			fs::rename(&v2_dir, &storage_dir_path).map_err(|_| BuildError::KVStoreSetupFailed)?;

			// fsync the renames
			fs::File::open(parent_dir)
				.and_then(|f| f.sync_all())
				.map_err(|_| BuildError::KVStoreSetupFailed)?;

			FilesystemStoreV2::new(storage_dir_path).map_err(|_| BuildError::KVStoreSetupFailed)
		},
		Err(_) => Err(BuildError::KVStoreSetupFailed),
	}
}

fn fs_store_sibling_path(storage_dir_path: &Path, file_name: &str) -> PathBuf {
	let mut sibling_path = storage_dir_path.to_path_buf();
	sibling_path.set_file_name(file_name);
	sibling_path
}

fn recover_incomplete_fs_store_migration(storage_dir_path: &Path) -> Result<(), BuildError> {
	let v2_dir = fs_store_sibling_path(storage_dir_path, "fs_store_v2_migrating");
	let backup_dir = fs_store_sibling_path(storage_dir_path, "fs_store_v1_backup");

	if storage_dir_path.exists() {
		if v2_dir.exists() {
			// The original store is still in place, so a temp migration dir is from a crash before
			// the rename step and can be discarded before retrying migration.
			fs::remove_dir_all(&v2_dir).map_err(|_| BuildError::KVStoreSetupFailed)?;
		}
		return Ok(());
	}

	if backup_dir.exists() {
		if v2_dir.exists() {
			// Prefer retrying from the v1 backup instead of deciding here whether the temp v2 dir is
			// usable. open_or_migrate_fs_store owns the actual v1-to-v2 migration.
			fs::remove_dir_all(&v2_dir).map_err(|_| BuildError::KVStoreSetupFailed)?;
		}
		// The crash happened after moving v1 aside; restore it so normal startup can migrate it.
		fs::rename(&backup_dir, storage_dir_path).map_err(|_| BuildError::KVStoreSetupFailed)?;
		return Ok(());
	}

	if v2_dir.exists() {
		// There is no v1 backup to retry from. Move the temp dir into place and let
		// open_or_migrate_fs_store decide whether it is a valid v2 store.
		fs::rename(&v2_dir, storage_dir_path).map_err(|_| BuildError::KVStoreSetupFailed)?;
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::{Path, PathBuf};

	use lightning::util::persist::{migrate_kv_store_data_async, KVStore};
	use lightning_persister::fs_store::v1::FilesystemStore;
	use lightning_persister::fs_store::v2::FilesystemStoreV2;

	use super::open_or_migrate_fs_store;
	use crate::io::test_utils::random_storage_path;

	const TEST_PRIMARY_NAMESPACE: &str = "test_primary_namespace";
	const TEST_SECONDARY_NAMESPACE: &str = "test_secondary_namespace";
	const TEST_KEY: &str = "test_key";
	const TEST_VALUE: &[u8] = b"test_value";

	#[tokio::test]
	async fn fs_store_migration_recovers_before_v1_backup_rename() {
		let fs_store_path = fs_store_path();
		let v1_store = write_v1_test_data(&fs_store_path).await;
		let v2_migrating_path = sibling_path(&fs_store_path, "fs_store_v2_migrating");
		let v2_store = FilesystemStoreV2::new(v2_migrating_path.clone()).unwrap();
		migrate_kv_store_data_async(&v1_store, &v2_store).await.unwrap();

		let migrated_store = open_or_migrate_fs_store(fs_store_path.clone()).await.unwrap();
		assert_eq!(
			KVStore::read(
				&migrated_store,
				TEST_PRIMARY_NAMESPACE,
				TEST_SECONDARY_NAMESPACE,
				TEST_KEY
			)
			.await
			.unwrap(),
			TEST_VALUE
		);
		assert!(fs_store_path.exists());
		assert!(!v2_migrating_path.exists());
	}

	#[tokio::test]
	async fn fs_store_migration_recovers_after_v1_backup_rename() {
		let fs_store_path = fs_store_path();
		let v1_store = write_v1_test_data(&fs_store_path).await;
		let v2_migrating_path = sibling_path(&fs_store_path, "fs_store_v2_migrating");
		let v2_store = FilesystemStoreV2::new(v2_migrating_path.clone()).unwrap();
		migrate_kv_store_data_async(&v1_store, &v2_store).await.unwrap();

		let backup_path = sibling_path(&fs_store_path, "fs_store_v1_backup");
		fs::rename(&fs_store_path, backup_path).unwrap();

		let migrated_store = open_or_migrate_fs_store(fs_store_path.clone()).await.unwrap();
		assert_eq!(
			KVStore::read(
				&migrated_store,
				TEST_PRIMARY_NAMESPACE,
				TEST_SECONDARY_NAMESPACE,
				TEST_KEY
			)
			.await
			.unwrap(),
			TEST_VALUE
		);
		assert!(fs_store_path.exists());
		assert!(!v2_migrating_path.exists());
	}

	#[tokio::test]
	async fn fs_store_migration_recovers_after_v2_rename() {
		let fs_store_path = fs_store_path();
		let v1_store = write_v1_test_data(&fs_store_path).await;
		let v2_migrating_path = sibling_path(&fs_store_path, "fs_store_v2_migrating");
		let v2_store = FilesystemStoreV2::new(v2_migrating_path.clone()).unwrap();
		migrate_kv_store_data_async(&v1_store, &v2_store).await.unwrap();

		let backup_path = sibling_path(&fs_store_path, "fs_store_v1_backup");
		fs::rename(&fs_store_path, &backup_path).unwrap();
		fs::rename(&v2_migrating_path, &fs_store_path).unwrap();

		let migrated_store = open_or_migrate_fs_store(fs_store_path.clone()).await.unwrap();
		assert_eq!(
			KVStore::read(
				&migrated_store,
				TEST_PRIMARY_NAMESPACE,
				TEST_SECONDARY_NAMESPACE,
				TEST_KEY
			)
			.await
			.unwrap(),
			TEST_VALUE
		);
		assert!(fs_store_path.exists());
		assert!(backup_path.exists());
		assert!(!v2_migrating_path.exists());
	}

	#[tokio::test]
	async fn fs_store_migration_recovers_backup_without_migrating_dir() {
		let fs_store_path = fs_store_path();
		write_v1_test_data(&fs_store_path).await;

		let backup_path = sibling_path(&fs_store_path, "fs_store_v1_backup");
		fs::rename(&fs_store_path, backup_path).unwrap();

		let migrated_store = open_or_migrate_fs_store(fs_store_path.clone()).await.unwrap();
		assert_eq!(
			KVStore::read(
				&migrated_store,
				TEST_PRIMARY_NAMESPACE,
				TEST_SECONDARY_NAMESPACE,
				TEST_KEY
			)
			.await
			.unwrap(),
			TEST_VALUE
		);
		assert!(fs_store_path.exists());
		assert!(!sibling_path(&fs_store_path, "fs_store_v1_backup").exists());
	}

	#[tokio::test]
	async fn fs_store_migration_recovers_unexpected_migrating_dir_without_backup() {
		let fs_store_path = fs_store_path();
		let v2_migrating_path = sibling_path(&fs_store_path, "fs_store_v2_migrating");
		let v2_store = FilesystemStoreV2::new(v2_migrating_path.clone()).unwrap();
		KVStore::write(
			&v2_store,
			TEST_PRIMARY_NAMESPACE,
			TEST_SECONDARY_NAMESPACE,
			TEST_KEY,
			TEST_VALUE.to_vec(),
		)
		.await
		.unwrap();

		let migrated_store = open_or_migrate_fs_store(fs_store_path.clone()).await.unwrap();
		assert_eq!(
			KVStore::read(
				&migrated_store,
				TEST_PRIMARY_NAMESPACE,
				TEST_SECONDARY_NAMESPACE,
				TEST_KEY
			)
			.await
			.unwrap(),
			TEST_VALUE
		);
		assert!(fs_store_path.exists());
		assert!(!v2_migrating_path.exists());
	}

	fn fs_store_path() -> PathBuf {
		let mut fs_store_path = random_storage_path();
		fs_store_path.push("fs_store");
		fs_store_path
	}

	fn sibling_path(path: &Path, file_name: &str) -> PathBuf {
		let mut sibling_path = path.to_path_buf();
		sibling_path.set_file_name(file_name);
		sibling_path
	}

	async fn write_v1_test_data(fs_store_path: &Path) -> FilesystemStore {
		let v1_store = FilesystemStore::new(fs_store_path.to_path_buf());
		KVStore::write(
			&v1_store,
			TEST_PRIMARY_NAMESPACE,
			TEST_SECONDARY_NAMESPACE,
			TEST_KEY,
			TEST_VALUE.to_vec(),
		)
		.await
		.unwrap();
		v1_store
	}
}
