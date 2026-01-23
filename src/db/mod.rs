use std::{
  collections::hash_map::DefaultHasher,
  env,
  fmt,
  fs,
  hash::{Hash, Hasher},
  io::{BufRead, BufReader, Read, Write},
  str,
  sync::OnceLock,
};

use base64::prelude::*;
use log::{debug, error, warn};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StashError {
  #[error("Input is empty or too large, skipping store.")]
  EmptyOrTooLarge,
  #[error("Input is all whitespace, skipping store.")]
  AllWhitespace,

  #[error("Failed to store entry: {0}")]
  Store(Box<str>),
  #[error("Entry excluded by app filter: {0}")]
  ExcludedByApp(Box<str>),
  #[error("Error reading entry during deduplication: {0}")]
  DeduplicationRead(Box<str>),
  #[error("Error decoding entry during deduplication: {0}")]
  DeduplicationDecode(Box<str>),
  #[error("Failed to remove entry during deduplication: {0}")]
  DeduplicationRemove(Box<str>),
  #[error("Failed to trim entry: {0}")]
  Trim(Box<str>),
  #[error("No entries to delete")]
  NoEntriesToDelete,
  #[error("Failed to delete last entry: {0}")]
  DeleteLast(Box<str>),
  #[error("Failed to wipe database: {0}")]
  Wipe(Box<str>),
  #[error("Failed to decode entry during list: {0}")]
  ListDecode(Box<str>),
  #[error("Failed to read input for decode: {0}")]
  DecodeRead(Box<str>),
  #[error("Failed to extract id for decode: {0}")]
  DecodeExtractId(Box<str>),
  #[error("Failed to get entry for decode: {0}")]
  DecodeGet(Box<str>),

  #[error("Failed to write decoded entry: {0}")]
  DecodeWrite(Box<str>),
  #[error("Failed to delete entry during query delete: {0}")]
  QueryDelete(Box<str>),
  #[error("Failed to delete entry with id {0}: {1}")]
  DeleteEntry(i64, Box<str>),
}

pub trait ClipboardDb {
  fn store_entry(
    &self,
    input: impl Read,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: Option<&[String]>,
  ) -> Result<i64, StashError>;

  fn deduplicate_by_hash(
    &self,
    content_hash: i64,
    max: u64,
  ) -> Result<usize, StashError>;
  fn trim_db(&self, max_items: u64) -> Result<(), StashError>;
  fn delete_last(&self) -> Result<(), StashError>;
  fn wipe_db(&self) -> Result<(), StashError>;
  fn list_entries(
    &self,
    out: impl Write,
    preview_width: u32,
    include_expired: bool,
  ) -> Result<usize, StashError>;
  fn decode_entry(
    &self,
    input: impl Read,
    out: impl Write,
    id_hint: Option<String>,
  ) -> Result<(), StashError>;
  fn delete_query(&self, query: &str) -> Result<usize, StashError>;
  fn delete_entries(&self, input: impl Read) -> Result<usize, StashError>;
  fn copy_entry(
    &self,
    id: i64,
  ) -> Result<(i64, Vec<u8>, Option<String>), StashError>;
}

#[derive(Serialize, Deserialize)]
pub struct Entry {
  pub contents: Vec<u8>,
  pub mime:     Option<String>,
}

impl fmt::Display for Entry {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let preview = preview_entry(&self.contents, self.mime.as_deref(), 100);
    write!(f, "{preview}")
  }
}

pub struct SqliteClipboardDb {
  pub conn: Connection,
}

impl SqliteClipboardDb {
  pub fn new(mut conn: Connection) -> Result<Self, StashError> {
    conn
      .pragma_update(None, "synchronous", "OFF")
      .map_err(|e| {
        StashError::Store(
          format!("Failed to set synchronous pragma: {e}").into(),
        )
      })?;
    conn
      .pragma_update(None, "journal_mode", "MEMORY")
      .map_err(|e| {
        StashError::Store(
          format!("Failed to set journal_mode pragma: {e}").into(),
        )
      })?;
    conn.pragma_update(None, "cache_size", "-256") // 256KB cache
      .map_err(|e| StashError::Store(format!("Failed to set cache_size pragma: {e}").into()))?;
    conn
      .pragma_update(None, "temp_store", "memory")
      .map_err(|e| {
        StashError::Store(
          format!("Failed to set temp_store pragma: {e}").into(),
        )
      })?;
    conn.pragma_update(None, "mmap_size", "0") // disable mmap
      .map_err(|e| StashError::Store(format!("Failed to set mmap_size pragma: {e}").into()))?;
    conn.pragma_update(None, "page_size", "512") // small(er) pages
      .map_err(|e| StashError::Store(format!("Failed to set page_size pragma: {e}").into()))?;

    let tx = conn.transaction().map_err(|e| {
      StashError::Store(
        format!("Failed to begin migration transaction: {e}").into(),
      )
    })?;

    let schema_version: i64 = tx
      .pragma_query_value(None, "user_version", |row| row.get(0))
      .map_err(|e| {
        StashError::Store(format!("Failed to read schema version: {e}").into())
      })?;

    if schema_version == 0 {
      tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                contents BLOB NOT NULL,
                mime TEXT
            );",
      )
      .map_err(|e| {
        StashError::Store(
          format!("Failed to create clipboard table: {e}").into(),
        )
      })?;

      tx.execute("PRAGMA user_version = 1", []).map_err(|e| {
        StashError::Store(format!("Failed to set schema version: {e}").into())
      })?;
    }

    // Add content_hash column if it doesn't exist
    // Migration MUST be done to avoid breaking existing installations.
    if schema_version < 2 {
      let has_content_hash: bool = tx
        .query_row(
          "SELECT sql FROM sqlite_master WHERE type='table' AND \
           name='clipboard'",
          [],
          |row| {
            let sql: String = row.get(0)?;
            Ok(sql.to_lowercase().contains("content_hash"))
          },
        )
        .unwrap_or(false);

      if !has_content_hash {
        tx.execute("ALTER TABLE clipboard ADD COLUMN content_hash INTEGER", [])
          .map_err(|e| {
            StashError::Store(
              format!("Failed to add content_hash column: {e}").into(),
            )
          })?;
      }

      // Create index for content_hash if it doesn't exist
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_content_hash ON \
         clipboard(content_hash)",
        [],
      )
      .map_err(|e| {
        StashError::Store(
          format!("Failed to create content_hash index: {e}").into(),
        )
      })?;

      tx.execute("PRAGMA user_version = 2", []).map_err(|e| {
        StashError::Store(format!("Failed to set schema version: {e}").into())
      })?;
    }

    // Add last_accessed column if it doesn't exist
    if schema_version < 3 {
      let has_last_accessed: bool = tx
        .query_row(
          "SELECT sql FROM sqlite_master WHERE type='table' AND \
           name='clipboard'",
          [],
          |row| {
            let sql: String = row.get(0)?;
            Ok(sql.to_lowercase().contains("last_accessed"))
          },
        )
        .unwrap_or(false);

      if !has_last_accessed {
        tx.execute("ALTER TABLE clipboard ADD COLUMN last_accessed INTEGER", [
        ])
        .map_err(|e| {
          StashError::Store(
            format!("Failed to add last_accessed column: {e}").into(),
          )
        })?;
      }

      // Create index for last_accessed if it doesn't exist
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_last_accessed ON \
         clipboard(last_accessed)",
        [],
      )
      .map_err(|e| {
        StashError::Store(
          format!("Failed to create last_accessed index: {e}").into(),
        )
      })?;

      tx.execute("PRAGMA user_version = 3", []).map_err(|e| {
        StashError::Store(format!("Failed to set schema version: {e}").into())
      })?;
    }

    // Add expires_at column if it doesn't exist (v4)
    if schema_version < 4 {
      let has_expires_at: bool = tx
        .query_row(
          "SELECT sql FROM sqlite_master WHERE type='table' AND \
           name='clipboard'",
          [],
          |row| {
            let sql: String = row.get(0)?;
            Ok(sql.to_lowercase().contains("expires_at"))
          },
        )
        .unwrap_or(false);

      if !has_expires_at {
        tx.execute("ALTER TABLE clipboard ADD COLUMN expires_at REAL", [])
          .map_err(|e| {
            StashError::Store(
              format!("Failed to add expires_at column: {e}").into(),
            )
          })?;
      }

      // Create partial index for expires_at (only index non-NULL values)
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_expires_at ON clipboard(expires_at) \
         WHERE expires_at IS NOT NULL",
        [],
      )
      .map_err(|e| {
        StashError::Store(
          format!("Failed to create expires_at index: {e}").into(),
        )
      })?;

      tx.execute("PRAGMA user_version = 4", []).map_err(|e| {
        StashError::Store(format!("Failed to set schema version: {e}").into())
      })?;
    }

    // Add is_expired column if it doesn't exist (v5)
    if schema_version < 5 {
      let has_is_expired: bool = tx
        .query_row(
          "SELECT sql FROM sqlite_master WHERE type='table' AND \
           name='clipboard'",
          [],
          |row| {
            let sql: String = row.get(0)?;
            Ok(sql.to_lowercase().contains("is_expired"))
          },
        )
        .unwrap_or(false);

      if !has_is_expired {
        tx.execute(
          "ALTER TABLE clipboard ADD COLUMN is_expired INTEGER DEFAULT 0",
          [],
        )
        .map_err(|e| {
          StashError::Store(
            format!("Failed to add is_expired column: {e}").into(),
          )
        })?;
      }

      // Create index for is_expired (for filtering)
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_is_expired ON clipboard(is_expired) \
         WHERE is_expired = 1",
        [],
      )
      .map_err(|e| {
        StashError::Store(
          format!("Failed to create is_expired index: {e}").into(),
        )
      })?;

      tx.execute("PRAGMA user_version = 5", []).map_err(|e| {
        StashError::Store(format!("Failed to set schema version: {e}").into())
      })?;
    }

    tx.commit().map_err(|e| {
      StashError::Store(
        format!("Failed to commit migration transaction: {e}").into(),
      )
    })?;

    // Initialize Wayland state in background thread. This will be used to track
    // focused window state.
    #[cfg(feature = "use-toplevel")]
    crate::wayland::init_wayland_state();
    Ok(Self { conn })
  }
}

impl SqliteClipboardDb {
  pub fn list_json(&self, include_expired: bool) -> Result<String, StashError> {
    let query = if include_expired {
      "SELECT id, contents, mime FROM clipboard ORDER BY \
       COALESCE(last_accessed, 0) DESC, id DESC"
    } else {
      "SELECT id, contents, mime FROM clipboard WHERE (is_expired IS NULL OR \
       is_expired = 0) ORDER BY COALESCE(last_accessed, 0) DESC, id DESC"
    };
    let mut stmt = self
      .conn
      .prepare(query)
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    let mut rows = stmt
      .query([])
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    let mut entries = Vec::new();

    while let Some(row) = rows
      .next()
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?
    {
      let id: i64 = row
        .get(0)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let contents: Vec<u8> = row
        .get(1)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let mime: Option<String> = row
        .get(2)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

      let contents_str = match mime.as_deref() {
        Some(m) if m.starts_with("text/") || m == "application/json" => {
          String::from_utf8_lossy(&contents).into_owned()
        },
        _ => base64::prelude::BASE64_STANDARD.encode(&contents),
      };
      entries.push(serde_json::json!({
          "id": id,
          "contents": contents_str,
          "mime": mime,
      }));
    }

    serde_json::to_string_pretty(&entries)
      .map_err(|e| StashError::ListDecode(e.to_string().into()))
  }
}

impl ClipboardDb for SqliteClipboardDb {
  fn store_entry(
    &self,
    mut input: impl Read,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: Option<&[String]>,
  ) -> Result<i64, StashError> {
    let mut buf = Vec::new();
    if input.read_to_end(&mut buf).is_err()
      || buf.is_empty()
      || buf.len() > 5 * 1_000_000
    {
      return Err(StashError::EmptyOrTooLarge);
    }
    if buf.iter().all(u8::is_ascii_whitespace) {
      return Err(StashError::AllWhitespace);
    }

    // Calculate content hash for deduplication
    let mut hasher = DefaultHasher::new();
    buf.hash(&mut hasher);
    #[allow(clippy::cast_possible_wrap)]
    let content_hash = hasher.finish() as i64;

    let mime = crate::mime::detect_mime(&buf);

    // Try to load regex from systemd credential file, then env var
    let regex = load_sensitive_regex();
    if let Some(re) = regex {
      // Only check text data
      if let Ok(s) = std::str::from_utf8(&buf)
        && re.is_match(s)
      {
        warn!("Clipboard entry matches sensitive regex, skipping store.");
        return Err(StashError::Store("Filtered by sensitive regex".into()));
      }
    }

    // Check if clipboard should be excluded based on running apps
    if should_exclude_by_app(excluded_apps) {
      warn!("Clipboard entry excluded by app filter");
      return Err(StashError::ExcludedByApp(
        "Clipboard entry from excluded app".into(),
      ));
    }

    self.deduplicate_by_hash(content_hash, max_dedupe_search)?;

    self
      .conn
      .execute(
        "INSERT INTO clipboard (contents, mime, content_hash, last_accessed) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
          buf,
          mime,
          content_hash,
          std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs() as i64
        ],
      )
      .map_err(|e| StashError::Store(e.to_string().into()))?;

    let id = self
      .conn
      .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
      .map_err(|e| StashError::Store(e.to_string().into()))?;

    self.trim_db(max_items)?;
    Ok(id)
  }

  fn deduplicate_by_hash(
    &self,
    content_hash: i64,
    max: u64,
  ) -> Result<usize, StashError> {
    let mut stmt = self
      .conn
      .prepare(
        "SELECT id FROM clipboard WHERE content_hash = ?1 ORDER BY id DESC \
         LIMIT ?2",
      )
      .map_err(|e| StashError::DeduplicationRead(e.to_string().into()))?;
    let mut rows = stmt
      .query(params![
        content_hash,
        i64::try_from(max).unwrap_or(i64::MAX)
      ])
      .map_err(|e| StashError::DeduplicationRead(e.to_string().into()))?;
    let mut deduped = 0;
    while let Some(row) = rows
      .next()
      .map_err(|e| StashError::DeduplicationRead(e.to_string().into()))?
    {
      let id: i64 = row
        .get(0)
        .map_err(|e| StashError::DeduplicationDecode(e.to_string().into()))?;
      self
        .conn
        .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
        .map_err(|e| StashError::DeduplicationRemove(e.to_string().into()))?;
      deduped += 1;
    }
    Ok(deduped)
  }

  fn trim_db(&self, max: u64) -> Result<(), StashError> {
    let count: i64 = self
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .map_err(|e| StashError::Trim(e.to_string().into()))?;
    let max_i64 = i64::try_from(max).unwrap_or(i64::MAX);
    if count > max_i64 {
      let to_delete = count - max_i64;

      #[allow(clippy::useless_conversion)]
      self
        .conn
        .execute(
          "DELETE FROM clipboard WHERE id IN (SELECT id FROM clipboard ORDER \
           BY id ASC LIMIT ?1)",
          params![i64::try_from(to_delete).unwrap_or(i64::MAX)],
        )
        .map_err(|e| StashError::Trim(e.to_string().into()))?;
    }
    Ok(())
  }

  fn delete_last(&self) -> Result<(), StashError> {
    let id: Option<i64> = self
      .conn
      .query_row(
        "SELECT id FROM clipboard ORDER BY id DESC LIMIT 1",
        [],
        |row| row.get(0),
      )
      .optional()
      .map_err(|e| StashError::DeleteLast(e.to_string().into()))?;
    if let Some(id) = id {
      self
        .conn
        .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
        .map_err(|e| StashError::DeleteLast(e.to_string().into()))?;
      Ok(())
    } else {
      Err(StashError::NoEntriesToDelete)
    }
  }

  fn wipe_db(&self) -> Result<(), StashError> {
    self
      .conn
      .execute("DELETE FROM clipboard", [])
      .map_err(|e| StashError::Wipe(e.to_string().into()))?;
    self
      .conn
      .execute("DELETE FROM sqlite_sequence WHERE name = 'clipboard'", [])
      .map_err(|e| StashError::Wipe(e.to_string().into()))?;
    Ok(())
  }

  fn list_entries(
    &self,
    mut out: impl Write,
    preview_width: u32,
    include_expired: bool,
  ) -> Result<usize, StashError> {
    let query = if include_expired {
      "SELECT id, contents, mime FROM clipboard ORDER BY \
       COALESCE(last_accessed, 0) DESC, id DESC"
    } else {
      "SELECT id, contents, mime FROM clipboard WHERE (is_expired IS NULL OR \
       is_expired = 0) ORDER BY COALESCE(last_accessed, 0) DESC, id DESC"
    };
    let mut stmt = self
      .conn
      .prepare(query)
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    let mut rows = stmt
      .query([])
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    let mut listed = 0;

    while let Some(row) = rows
      .next()
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?
    {
      let id: i64 = row
        .get(0)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let contents: Vec<u8> = row
        .get(1)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let mime: Option<String> = row
        .get(2)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

      let preview = preview_entry(&contents, mime.as_deref(), preview_width);
      if writeln!(out, "{id}\t{preview}").is_ok() {
        listed += 1;
      }
    }
    Ok(listed)
  }

  fn decode_entry(
    &self,
    input: impl Read,
    mut out: impl Write,
    id_hint: Option<String>,
  ) -> Result<(), StashError> {
    let input_str = if let Some(s) = id_hint {
      s
    } else {
      let mut input = BufReader::new(input);
      let mut buf = String::new();
      input
        .read_to_string(&mut buf)
        .map_err(|e| StashError::DecodeExtractId(e.to_string().into()))?;
      buf
    };
    let id: i64 = extract_id(&input_str)
      .map_err(|e| StashError::DecodeExtractId(e.into()))?;
    let (contents, _mime): (Vec<u8>, Option<String>) = self
      .conn
      .query_row(
        "SELECT contents, mime FROM clipboard WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .map_err(|e| StashError::DecodeGet(e.to_string().into()))?;
    out
      .write_all(&contents)
      .map_err(|e| StashError::DecodeWrite(e.to_string().into()))?;
    log::info!("Decoded entry with id {id}");
    Ok(())
  }

  fn delete_query(&self, query: &str) -> Result<usize, StashError> {
    let mut stmt = self
      .conn
      .prepare("SELECT id, contents FROM clipboard")
      .map_err(|e| StashError::QueryDelete(e.to_string().into()))?;
    let mut rows = stmt
      .query([])
      .map_err(|e| StashError::QueryDelete(e.to_string().into()))?;
    let mut deleted = 0;
    while let Some(row) = rows
      .next()
      .map_err(|e| StashError::QueryDelete(e.to_string().into()))?
    {
      let id: i64 = row
        .get(0)
        .map_err(|e| StashError::QueryDelete(e.to_string().into()))?;
      let contents: Vec<u8> = row
        .get(1)
        .map_err(|e| StashError::QueryDelete(e.to_string().into()))?;
      if contents.windows(query.len()).any(|w| w == query.as_bytes()) {
        self
          .conn
          .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
          .map_err(|e| StashError::QueryDelete(e.to_string().into()))?;
        deleted += 1;
      }
    }
    Ok(deleted)
  }

  fn delete_entries(&self, in_: impl Read) -> Result<usize, StashError> {
    let reader = BufReader::new(in_);
    let mut deleted = 0;
    for line in reader.lines().map_while(Result::ok) {
      if let Ok(id) = extract_id(&line) {
        self
          .conn
          .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
          .map_err(|e| StashError::DeleteEntry(id, e.to_string().into()))?;
        deleted += 1;
      }
    }
    Ok(deleted)
  }

  fn copy_entry(
    &self,
    id: i64,
  ) -> Result<(i64, Vec<u8>, Option<String>), StashError> {
    let (contents, mime, content_hash): (Vec<u8>, Option<String>, Option<i64>) =
      self
        .conn
        .query_row(
          "SELECT contents, mime, content_hash FROM clipboard WHERE id = ?1",
          params![id],
          |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| StashError::DecodeGet(e.to_string().into()))?;

    if let Some(hash) = content_hash {
      let most_recent_id: Option<i64> = self
        .conn
        .query_row(
          "SELECT id FROM clipboard WHERE content_hash = ?1 AND last_accessed \
           = (SELECT MAX(last_accessed) FROM clipboard WHERE content_hash = \
           ?1)",
          params![hash],
          |row| row.get(0),
        )
        .optional()
        .map_err(|e| StashError::DecodeGet(e.to_string().into()))?;

      if most_recent_id != Some(id) {
        self
          .conn
          .execute(
            "UPDATE clipboard SET last_accessed = CAST(strftime('%s', 'now') \
             AS INTEGER) WHERE id = ?1",
            params![id],
          )
          .map_err(|e| StashError::Store(e.to_string().into()))?;
      }
    }

    Ok((id, contents, mime))
  }
}

impl SqliteClipboardDb {
  /// Get current Unix timestamp with sub-second precision
  pub fn now() -> f64 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs_f64()
  }

  /// Clean up all expired entries. Returns count deleted.
  pub fn cleanup_expired(&self) -> Result<usize, StashError> {
    let now = Self::now();
    self
      .conn
      .execute(
        "DELETE FROM clipboard WHERE expires_at IS NOT NULL AND expires_at <= \
         ?1",
        [now],
      )
      .map_err(|e| StashError::Trim(e.to_string().into()))
  }

  /// Get the earliest expiration (timestamp, id) for heap initialization
  pub fn get_next_expiration(&self) -> Result<Option<(f64, i64)>, StashError> {
    match self.conn.query_row(
      "SELECT expires_at, id FROM clipboard WHERE expires_at IS NOT NULL \
       ORDER BY expires_at ASC LIMIT 1",
      [],
      |row| Ok((row.get(0)?, row.get(1)?)),
    ) {
      Ok(result) => Ok(Some(result)),
      Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
      Err(e) => Err(StashError::Store(e.to_string().into())),
    }
  }

  /// Set expiration timestamp for an entry
  pub fn set_expiration(
    &self,
    id: i64,
    expires_at: f64,
  ) -> Result<(), StashError> {
    self
      .conn
      .execute(
        "UPDATE clipboard SET expires_at = ?2 WHERE id = ?1",
        params![id, expires_at],
      )
      .map_err(|e| StashError::Store(e.to_string().into()))?;
    Ok(())
  }

  /// Optimize database using VACUUM
  pub fn vacuum(&self) -> Result<(), StashError> {
    self
      .conn
      .execute("VACUUM", [])
      .map_err(|e| StashError::Store(e.to_string().into()))?;
    Ok(())
  }

  /// Get database statistics
  pub fn stats(&self) -> Result<String, StashError> {
    let total: i64 = self
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    let expired: i64 = self
      .conn
      .query_row(
        "SELECT COUNT(*) FROM clipboard WHERE is_expired = 1",
        [],
        |row| row.get(0),
      )
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    let active = total - expired;

    let with_expiration: i64 = self
      .conn
      .query_row(
        "SELECT COUNT(*) FROM clipboard WHERE expires_at IS NOT NULL AND \
         (is_expired IS NULL OR is_expired = 0)",
        [],
        |row| row.get(0),
      )
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    // Get database file size
    let page_count: i64 = self
      .conn
      .query_row("PRAGMA page_count", [], |row| row.get(0))
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    let page_size: i64 = self
      .conn
      .query_row("PRAGMA page_size", [], |row| row.get(0))
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    let size_bytes = page_count * page_size;
    let size_mb = size_bytes as f64 / 1024.0 / 1024.0;

    Ok(format!(
      "Database Statistics:\n\nEntries:\nTotal:      {total}\nActive:     \
       {active}\nExpired:    {expired}\nWith TTL:   \
       {with_expiration}\n\nStorage:\nSize:       {size_mb:.2} MB \
       ({size_bytes} bytes)\nPages:      {page_count}\nPage size:  \
       {page_size} bytes"
    ))
  }
}

/// Try to load a sensitive regex from systemd credential or env.
///
/// # Returns
///
///  `Some(Regex)` if present and valid, `None` otherwise.
fn load_sensitive_regex() -> Option<Regex> {
  static REGEX_CACHE: OnceLock<Option<Regex>> = OnceLock::new();
  static CHECKED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

  if !CHECKED.load(std::sync::atomic::Ordering::Relaxed) {
    CHECKED.store(true, std::sync::atomic::Ordering::Relaxed);

    let regex = if let Ok(regex_path) = env::var("CREDENTIALS_DIRECTORY") {
      let file = format!("{regex_path}/clipboard_filter");
      if let Ok(contents) = fs::read_to_string(&file) {
        Regex::new(contents.trim()).ok()
      } else {
        None
      }
    } else if let Ok(pattern) = env::var("STASH_SENSITIVE_REGEX") {
      Regex::new(&pattern).ok()
    } else {
      None
    };

    let _ = REGEX_CACHE.set(regex);
  }

  REGEX_CACHE.get().and_then(std::clone::Clone::clone)
}

pub fn extract_id(input: &str) -> Result<i64, &'static str> {
  let id_str = input.split('\t').next().unwrap_or("");
  id_str.parse().map_err(|_| "invalid id")
}

pub fn preview_entry(data: &[u8], mime: Option<&str>, width: u32) -> String {
  if let Some(mime) = mime {
    if mime.starts_with("image/") {
      return format!("[[ binary data {} {} ]]", size_str(data.len()), mime);
    } else if mime == "application/json" || mime.starts_with("text/") {
      let Ok(s) = str::from_utf8(data) else {
        return format!("[[ invalid UTF-8 {} ]]", size_str(data.len()));
      };

      let trimmed = s.trim();
      if trimmed.len() <= width as usize
        && !trimmed.chars().any(|c| c.is_whitespace() && c != ' ')
      {
        return trimmed.to_string();
      }

      // Only allocate new string if we need to replace whitespace
      let mut result = String::with_capacity(width as usize + 1);
      for (char_count, c) in trimmed.chars().enumerate() {
        if char_count >= width as usize {
          result.push('…');
          break;
        }

        if c.is_whitespace() {
          result.push(' ');
        } else {
          result.push(c);
        }
      }
      return result;
    }
  }

  // For non-text data, use lossy conversion
  let s = String::from_utf8_lossy(data);
  truncate(s.trim(), width as usize, "…")
}

pub fn truncate(s: &str, max: usize, ellip: &str) -> String {
  let char_count = s.chars().count();
  if char_count > max {
    let mut result = String::with_capacity(max * 4 + ellip.len()); // UTF-8 worst case
    let mut char_iter = s.chars();
    for _ in 0..max {
      if let Some(c) = char_iter.next() {
        result.push(c);
      }
    }
    result.push_str(ellip);
    result
  } else {
    s.to_string()
  }
}

pub fn size_str(size: usize) -> String {
  let units = ["B", "KiB", "MiB"];
  let mut fsize = if let Ok(val) = u32::try_from(size) {
    f64::from(val)
  } else {
    error!("Clipboard entry size too large for display: {size}");
    f64::from(u32::MAX)
  };
  let mut i = 0;
  while fsize >= 1024.0 && i < units.len() - 1 {
    fsize /= 1024.0;
    i += 1;
  }
  format!("{:.0} {}", fsize, units[i])
}

/// Check if clipboard should be excluded based on excluded apps configuration.
/// Uses timing correlation and focused window detection to identify source app.
fn should_exclude_by_app(excluded_apps: Option<&[String]>) -> bool {
  let excluded = match excluded_apps {
    Some(apps) if !apps.is_empty() => apps,
    _ => return false,
  };

  // Try multiple detection strategies
  if detect_excluded_app_activity(excluded) {
    return true;
  }

  false
}

/// Detect if clipboard likely came from an excluded app using multiple
/// strategies.
fn detect_excluded_app_activity(excluded_apps: &[String]) -> bool {
  debug!("Checking clipboard exclusion against: {excluded_apps:?}");

  // Strategy 1: Check focused window (compositor-dependent)
  if let Some(focused_app) = get_focused_window_app() {
    debug!("Focused window detected: {focused_app}");
    if app_matches_exclusion(&focused_app, excluded_apps) {
      debug!("Clipboard excluded: focused window matches {focused_app}");
      return true;
    }
  } else {
    debug!("No focused window detected");
  }

  // Strategy 2: Check recently active processes (timing correlation)
  if let Some(active_app) = get_recently_active_excluded_app(excluded_apps) {
    debug!("Clipboard excluded: recent activity from {active_app}");
    return true;
  }
  debug!("No recently active excluded apps found");

  debug!("Clipboard not excluded");
  false
}

/// Try to get the currently focused window application name.
fn get_focused_window_app() -> Option<String> {
  // Try Wayland protocol first
  #[cfg(feature = "use-toplevel")]
  if let Some(app) = crate::wayland::get_focused_window_app() {
    return Some(app);
  }

  // Fallback: Check WAYLAND_CLIENT_NAME environment variable
  if let Ok(client) = env::var("WAYLAND_CLIENT_NAME")
    && !client.is_empty()
  {
    debug!("Found WAYLAND_CLIENT_NAME: {client}");
    return Some(client);
  }

  debug!("No focused window detection method worked");
  None
}

/// Check for recently active excluded apps using CPU and I/O activity.
fn get_recently_active_excluded_app(
  excluded_apps: &[String],
) -> Option<String> {
  let proc_dir = std::path::Path::new("/proc");
  if !proc_dir.exists() {
    return None;
  }

  let mut candidates = Vec::new();

  if let Ok(entries) = std::fs::read_dir(proc_dir) {
    for entry in entries.flatten() {
      if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>()
        && let Ok(comm) = fs::read_to_string(format!("/proc/{pid}/comm"))
      {
        let process_name = comm.trim();

        // Check process name against exclusion list
        if app_matches_exclusion(process_name, excluded_apps)
          && has_recent_activity(pid)
        {
          candidates
            .push((process_name.to_string(), get_process_activity_score(pid)));
        }
      }
    }
  }

  // Return the most active excluded app
  candidates
    .into_iter()
    .max_by_key(|(_, score)| *score)
    .map(|(name, _)| name)
}

/// Check if a process has had recent activity (simple heuristic).
fn has_recent_activity(pid: u32) -> bool {
  // Check /proc/PID/stat for recent CPU usage
  if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
    let fields: Vec<&str> = stat.split_whitespace().collect();
    if fields.len() > 14 {
      // Fields 14 and 15 are utime and stime
      if let (Ok(utime), Ok(stime)) =
        (fields[13].parse::<u64>(), fields[14].parse::<u64>())
      {
        let total_time = utime + stime;
        // Simple heuristic: if process has any significant CPU time, consider
        // it active
        return total_time > 100; // arbitrary threshold
      }
    }
  }

  // Check /proc/PID/io for recent I/O activity
  if let Ok(io_stats) = fs::read_to_string(format!("/proc/{pid}/io")) {
    for line in io_stats.lines() {
      if (line.starts_with("write_bytes:") || line.starts_with("read_bytes:"))
        && let Some(value_str) = line.split(':').nth(1)
        && let Ok(value) = value_str.trim().parse::<u64>()
        && value > 1024 * 1024
      {
        // 1MB threshold
        return true;
      }
    }
  }

  false
}

/// Get a simple activity score for process prioritization.
fn get_process_activity_score(pid: u32) -> u64 {
  let mut score = 0;

  // Add CPU time to score
  if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
    let fields: Vec<&str> = stat.split_whitespace().collect();
    if fields.len() > 14
      && let (Ok(utime), Ok(stime)) =
        (fields[13].parse::<u64>(), fields[14].parse::<u64>())
    {
      score += utime + stime;
    }
  }

  // Add I/O activity to score
  if let Ok(io_stats) = fs::read_to_string(format!("/proc/{pid}/io")) {
    for line in io_stats.lines() {
      if (line.starts_with("write_bytes:") || line.starts_with("read_bytes:"))
        && let Some(value_str) = line.split(':').nth(1)
        && let Ok(value) = value_str.trim().parse::<u64>()
      {
        score += value / 1024; // convert to KB
      }
    }
  }

  score
}

/// Check if an app name matches any in the exclusion list.
/// Supports basic string matching and simple regex patterns.
fn app_matches_exclusion(app_name: &str, excluded_apps: &[String]) -> bool {
  debug!("Checking if '{app_name}' matches exclusion list: {excluded_apps:?}");

  for excluded in excluded_apps {
    // Basic string matching (case-insensitive)
    if app_name.to_lowercase() == excluded.to_lowercase() {
      debug!("Matched exact string: {app_name} == {excluded}");
      return true;
    }

    // Simple pattern matching for common cases
    if excluded.starts_with('^') && excluded.ends_with('$') {
      // Exact match pattern like ^AppName$
      let pattern = &excluded[1..excluded.len() - 1];
      if app_name == pattern {
        debug!("Matched exact pattern: {app_name} == {pattern}");
        return true;
      }
    } else if excluded.contains('*') {
      // Simple wildcard matching
      let pattern = excluded.replace('*', ".*");
      if let Ok(regex) = regex::Regex::new(&pattern)
        && regex.is_match(app_name)
      {
        debug!("Matched wildcard pattern: {app_name} matches {excluded}");
        return true;
      }
    }
  }

  debug!("No match found for '{app_name}'");
  false
}

#[cfg(test)]
mod tests {
  use rusqlite::Connection;

  use super::*;

  fn get_schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
  }

  fn table_column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let query = format!(
      "SELECT sql FROM sqlite_master WHERE type='table' AND name='{}'",
      table
    );
    match conn.query_row(&query, [], |row| row.get::<_, String>(0)) {
      Ok(sql) => sql.contains(column),
      Err(_) => false,
    }
  }

  fn index_exists(conn: &Connection, index: &str) -> bool {
    let query = "SELECT name FROM sqlite_master WHERE type='index' AND name=?1";
    conn
      .query_row(query, [index], |row| row.get::<_, String>(0))
      .is_ok()
  }

  #[test]
  fn test_fresh_database_v3_schema() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_fresh.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");

    let db = SqliteClipboardDb::new(conn).expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn).expect("Failed to get schema version"),
      5
    );

    assert!(table_column_exists(&db.conn, "clipboard", "content_hash"));
    assert!(table_column_exists(&db.conn, "clipboard", "last_accessed"));

    assert!(index_exists(&db.conn, "idx_content_hash"));
    assert!(index_exists(&db.conn, "idx_last_accessed"));

    db.conn
      .execute(
        "INSERT INTO clipboard (contents, mime, content_hash, last_accessed) \
         VALUES (x'010203', 'text/plain', 12345, 1704067200)",
        [],
      )
      .expect("Failed to insert test data");

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1);
  }

  #[test]
  fn test_migration_from_v0() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_v0.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");

    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (id INTEGER PRIMARY KEY \
         AUTOINCREMENT, contents BLOB NOT NULL, mime TEXT);",
      )
      .expect("Failed to create table");

    conn
      .execute_batch(
        "INSERT INTO clipboard (contents, mime) VALUES (x'010203', \
         'text/plain')",
      )
      .expect("Failed to insert data");

    assert_eq!(get_schema_version(&conn).expect("Failed to get version"), 0);

    let db = SqliteClipboardDb::new(conn).expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn)
        .expect("Failed to get version after migration"),
      5
    );

    assert!(table_column_exists(&db.conn, "clipboard", "content_hash"));
    assert!(table_column_exists(&db.conn, "clipboard", "last_accessed"));

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1, "Existing data should be preserved");
  }

  #[test]
  fn test_migration_from_v1() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_v1.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");

    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (id INTEGER PRIMARY KEY \
         AUTOINCREMENT, contents BLOB NOT NULL, mime TEXT);",
      )
      .expect("Failed to create table");

    conn
      .pragma_update(None, "user_version", 1i64)
      .expect("Failed to set version");

    conn
      .execute_batch(
        "INSERT INTO clipboard (contents, mime) VALUES (x'010203', \
         'text/plain')",
      )
      .expect("Failed to insert data");

    let db = SqliteClipboardDb::new(conn).expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn)
        .expect("Failed to get version after migration"),
      5
    );

    assert!(table_column_exists(&db.conn, "clipboard", "content_hash"));
    assert!(table_column_exists(&db.conn, "clipboard", "last_accessed"));

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1, "Existing data should be preserved");
  }

  #[test]
  fn test_migration_from_v2() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_v2.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");

    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (id INTEGER PRIMARY KEY \
         AUTOINCREMENT, contents BLOB NOT NULL, mime TEXT, content_hash \
         INTEGER);",
      )
      .expect("Failed to create table");

    conn
      .pragma_update(None, "user_version", 2i64)
      .expect("Failed to set version");

    conn
      .execute_batch(
        "INSERT INTO clipboard (contents, mime, content_hash) VALUES \
         (x'010203', 'text/plain', 12345)",
      )
      .expect("Failed to insert data");

    let db = SqliteClipboardDb::new(conn).expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn)
        .expect("Failed to get version after migration"),
      5
    );

    assert!(table_column_exists(&db.conn, "clipboard", "last_accessed"));
    assert!(index_exists(&db.conn, "idx_last_accessed"));

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1, "Existing data should be preserved");
  }

  #[test]
  fn test_idempotent_migration() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_idempotent.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");

    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (id INTEGER PRIMARY KEY \
         AUTOINCREMENT, contents BLOB NOT NULL, mime TEXT);",
      )
      .expect("Failed to create table");

    let db = SqliteClipboardDb::new(conn).expect("Failed to create database");
    let version_after_first =
      get_schema_version(&db.conn).expect("Failed to get version");

    let db2 =
      SqliteClipboardDb::new(db.conn).expect("Failed to create database again");
    let version_after_second =
      get_schema_version(&db2.conn).expect("Failed to get version");

    assert_eq!(version_after_first, version_after_second);
    assert_eq!(version_after_first, 5);
  }

  #[test]
  fn test_store_and_retrieve_with_new_columns() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_store.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");
    let db = SqliteClipboardDb::new(conn).expect("Failed to create database");

    let test_data = b"Hello, World!";
    let cursor = std::io::Cursor::new(test_data.to_vec());

    let id = db
      .store_entry(cursor, 100, 1000, None)
      .expect("Failed to store entry");

    let content_hash: Option<i64> = db
      .conn
      .query_row(
        "SELECT content_hash FROM clipboard WHERE id = ?1",
        [id],
        |row| row.get(0),
      )
      .expect("Failed to get content_hash");

    let last_accessed: Option<i64> = db
      .conn
      .query_row(
        "SELECT last_accessed FROM clipboard WHERE id = ?1",
        [id],
        |row| row.get(0),
      )
      .expect("Failed to get last_accessed");

    assert!(content_hash.is_some(), "content_hash should be set");
    assert!(last_accessed.is_some(), "last_accessed should be set");
  }

  #[test]
  fn test_last_accessed_updated_on_copy() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_copy.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");
    let db = SqliteClipboardDb::new(conn).expect("Failed to create database");

    let test_data = b"Test content for copy";
    let cursor = std::io::Cursor::new(test_data.to_vec());
    let id_a = db
      .store_entry(cursor, 100, 1000, None)
      .expect("Failed to store entry A");

    let original_last_accessed: i64 = db
      .conn
      .query_row(
        "SELECT last_accessed FROM clipboard WHERE id = ?1",
        [id_a],
        |row| row.get(0),
      )
      .expect("Failed to get last_accessed");

    std::thread::sleep(std::time::Duration::from_millis(1100));

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    test_data.hash(&mut hasher);
    let content_hash = hasher.finish() as i64;

    let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .expect("Time went backwards")
      .as_secs() as i64;

    db.conn
      .execute(
        "INSERT INTO clipboard (contents, mime, content_hash, last_accessed) \
         VALUES (?1, ?2, ?3, ?4)",
        params![test_data as &[u8], "text/plain", content_hash, now],
      )
      .expect("Failed to insert entry B directly");

    std::thread::sleep(std::time::Duration::from_millis(1100));

    let (..) = db.copy_entry(id_a).expect("Failed to copy entry");

    let new_last_accessed: i64 = db
      .conn
      .query_row(
        "SELECT last_accessed FROM clipboard WHERE id = ?1",
        [id_a],
        |row| row.get(0),
      )
      .expect("Failed to get updated last_accessed");

    assert!(
      new_last_accessed > original_last_accessed,
      "last_accessed should be updated when copying an entry that is not the \
       most recent"
    );
  }

  #[test]
  fn test_migration_with_existing_columns_but_v0() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_v0_with_cols.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");

    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (id INTEGER PRIMARY KEY \
         AUTOINCREMENT, contents BLOB NOT NULL, mime TEXT, content_hash \
         INTEGER, last_accessed INTEGER);",
      )
      .expect("Failed to create table with all columns");

    conn
      .pragma_update(None, "user_version", 0i64)
      .expect("Failed to set version to 0");

    conn
      .execute_batch(
        "INSERT INTO clipboard (contents, mime, content_hash, last_accessed) \
         VALUES (x'010203', 'text/plain', 12345, 1704067200)",
      )
      .expect("Failed to insert data");

    let db = SqliteClipboardDb::new(conn).expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn).expect("Failed to get version"),
      5
    );

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1, "Existing data should be preserved");
  }
}
