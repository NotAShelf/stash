use std::path::PathBuf;

use rusqlite::OptionalExtension;

use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

/// Async wrapper for database operations that runs blocking operations
/// on a thread pool to avoid blocking the async runtime. Since
/// [`rusqlite::Connection`] is not Send, we store the database path and open a
/// new connection for each operation.
pub struct AsyncClipboardDb {
  db_path: PathBuf,
}

impl AsyncClipboardDb {
  pub fn new(db_path: PathBuf) -> Self {
    Self { db_path }
  }

  pub async fn store_entry(
    &self,
    data: Vec<u8>,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: Option<Vec<String>>,
    min_size: Option<usize>,
    max_size: usize,
  ) -> Result<i64, StashError> {
    let path = self.db_path.clone();
    blocking::unblock(move || {
      let db = Self::open_db_internal(&path)?;
      db.store_entry(
        std::io::Cursor::new(data),
        max_dedupe_search,
        max_items,
        excluded_apps.as_deref(),
        min_size,
        max_size,
      )
    })
    .await
  }

  pub async fn set_expiration(
    &self,
    id: i64,
    expires_at: f64,
  ) -> Result<(), StashError> {
    let path = self.db_path.clone();
    blocking::unblock(move || {
      let db = Self::open_db_internal(&path)?;
      db.set_expiration(id, expires_at)
    })
    .await
  }

  pub async fn load_all_expirations(
    &self,
  ) -> Result<Vec<(f64, i64)>, StashError> {
    let path = self.db_path.clone();
    blocking::unblock(move || {
      let db = Self::open_db_internal(&path)?;
      let mut stmt = db
        .conn
        .prepare(
          "SELECT expires_at, id FROM clipboard WHERE expires_at IS NOT NULL \
           AND (is_expired IS NULL OR is_expired = 0) ORDER BY expires_at ASC",
        )
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

      let mut rows = stmt
        .query([])
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let mut expirations = Vec::new();

      while let Some(row) = rows
        .next()
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?
      {
        let exp = row
          .get::<_, f64>(0)
          .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
        let id = row
          .get::<_, i64>(1)
          .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
        expirations.push((exp, id));
      }
      Ok(expirations)
    })
    .await
  }

  pub async fn get_content_hash(
    &self,
    id: i64,
  ) -> Result<Option<i64>, StashError> {
    let path = self.db_path.clone();
    blocking::unblock(move || {
      let db = Self::open_db_internal(&path)?;
      let result: Option<i64> = db
        .conn
        .query_row(
          "SELECT content_hash FROM clipboard WHERE id = ?1",
          [id],
          |row| row.get(0),
        )
        .optional()
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      Ok(result)
    })
    .await
  }

  pub async fn mark_expired(&self, id: i64) -> Result<(), StashError> {
    let path = self.db_path.clone();
    blocking::unblock(move || {
      let db = Self::open_db_internal(&path)?;
      db.conn
        .execute("UPDATE clipboard SET is_expired = 1 WHERE id = ?1", [id])
        .map_err(|e| StashError::Store(e.to_string().into()))?;
      Ok(())
    })
    .await
  }

  fn open_db_internal(path: &PathBuf) -> Result<SqliteClipboardDb, StashError> {
    let conn = rusqlite::Connection::open(path).map_err(|e| {
      StashError::Store(format!("Failed to open database: {e}").into())
    })?;
    SqliteClipboardDb::new(conn, path.clone())
  }
}

impl Clone for AsyncClipboardDb {
  fn clone(&self) -> Self {
    Self {
      db_path: self.db_path.clone(),
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

  use tempfile::tempdir;

  use super::*;

  fn setup_test_db() -> (AsyncClipboardDb, tempfile::TempDir) {
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test.db");

    // Create initial database
    {
      let conn =
        rusqlite::Connection::open(&db_path).expect("Failed to open database");
      crate::db::SqliteClipboardDb::new(conn, db_path.clone())
        .expect("Failed to create database");
    }

    let async_db = AsyncClipboardDb::new(db_path);
    (async_db, temp_dir)
  }

  #[test]
  fn test_async_store_entry() {
    smol::block_on(async {
      let (async_db, _temp_dir) = setup_test_db();
      let data = b"async test data";

      let id = async_db
        .store_entry(data.to_vec(), 100, 1000, None, None, 5_000_000)
        .await
        .expect("Failed to store entry");

      assert!(id > 0, "Should return positive id");

      // Verify it was stored by checking content hash
      let hash = async_db
        .get_content_hash(id)
        .await
        .expect("Failed to get hash")
        .expect("Hash should exist");

      // Calculate expected hash
      let mut hasher = crate::db::Fnv1aHasher::new();
      hasher.write(data);
      let expected_hash = hasher.finish() as i64;

      assert_eq!(hash, expected_hash, "Stored hash should match");
    });
  }

  #[test]
  fn test_async_set_expiration_and_load() {
    smol::block_on(async {
      let (async_db, _temp_dir) = setup_test_db();
      let data = b"expiring entry";

      let id = async_db
        .store_entry(data.to_vec(), 100, 1000, None, None, 5_000_000)
        .await
        .expect("Failed to store entry");

      let expires_at = 1234567890.5;
      async_db
        .set_expiration(id, expires_at)
        .await
        .expect("Failed to set expiration");

      // Load all expirations
      let expirations = async_db
        .load_all_expirations()
        .await
        .expect("Failed to load expirations");

      assert_eq!(expirations.len(), 1, "Should have one expiration");
      assert!(
        (expirations[0].0 - expires_at).abs() < 0.001,
        "Expiration time should match"
      );
      assert_eq!(expirations[0].1, id, "Expiration id should match");
    });
  }

  #[test]
  fn test_async_mark_expired() {
    smol::block_on(async {
      let (async_db, _temp_dir) = setup_test_db();
      let data = b"entry to expire";

      let id = async_db
        .store_entry(data.to_vec(), 100, 1000, None, None, 5_000_000)
        .await
        .expect("Failed to store entry");

      async_db
        .mark_expired(id)
        .await
        .expect("Failed to mark as expired");

      // Load expirations, this should be empty since entry is now marked
      // expired
      let expirations = async_db
        .load_all_expirations()
        .await
        .expect("Failed to load expirations");

      assert!(
        expirations.is_empty(),
        "Expired entries should not be loaded"
      );
    });
  }

  #[test]
  fn test_async_get_content_hash_not_found() {
    smol::block_on(async {
      let (async_db, _temp_dir) = setup_test_db();

      let hash = async_db
        .get_content_hash(999999)
        .await
        .expect("Should not fail on non-existent entry");

      assert!(hash.is_none(), "Hash should be None for non-existent entry");
    });
  }

  #[test]
  fn test_async_clone() {
    let (async_db, _temp_dir) = setup_test_db();
    let cloned = async_db.clone();

    smol::block_on(async {
      // Both should work independently
      let data = b"clone test";

      let id1 = async_db
        .store_entry(data.to_vec(), 100, 1000, None, None, 5_000_000)
        .await
        .expect("Failed with original");

      let id2 = cloned
        .store_entry(data.to_vec(), 100, 1000, None, None, 5_000_000)
        .await
        .expect("Failed with clone");

      assert_ne!(id1, id2, "Should store as separate entries");
    });
  }

  #[test]
  fn test_async_concurrent_operations() {
    smol::block_on(async {
      let (async_db, _temp_dir) = setup_test_db();

      // Spawn multiple concurrent store operations
      let futures: Vec<_> = (0..5)
        .map(|i| {
          let db = async_db.clone();
          let data = format!("concurrent test {}", i).into_bytes();
          smol::spawn(async move {
            db.store_entry(data, 100, 1000, None, None, 5_000_000).await
          })
        })
        .collect();

      let results: Result<Vec<_>, _> = futures::future::join_all(futures)
        .await
        .into_iter()
        .collect();

      let ids = results.expect("All stores should succeed");
      assert_eq!(ids.len(), 5, "Should have 5 entries");

      // All IDs should be unique
      let unique_ids: HashSet<_> = ids.iter().collect();
      assert_eq!(unique_ids.len(), 5, "All IDs should be unique");
    });
  }
}
