use std::path::PathBuf;

use rusqlite::OptionalExtension;

use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

/// Async wrapper for database operations that runs blocking operations
/// on a thread pool to avoid blocking the async runtime.
///
/// Since rusqlite::Connection is not Send, we store the database path
/// and open a new connection for each operation.
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
