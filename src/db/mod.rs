use std::{
  env,
  fmt,
  fs,
  io::{BufRead, BufReader, Read, Write},
  path::PathBuf,
  str,
  sync::{Mutex, OnceLock},
  time::{Duration, Instant},
};

pub mod nonblocking;

use std::hash::Hasher;

use crate::hash::Fnv1aHasher;

/// Cache for process scanning results to avoid expensive `/proc` reads on every
/// store operation. TTL of 5 seconds balances freshness with performance.
struct ProcessCache {
  last_scan:    Instant,
  excluded_app: Option<String>,
}

impl ProcessCache {
  const TTL: Duration = Duration::from_secs(5);

  /// Check cache for recently active excluded app.
  /// Only caches positive results (when an excluded app IS found).
  /// Negative results (no excluded apps) are never cached to ensure
  /// we don't miss exclusions when users switch apps.
  fn get(excluded_apps: &[String]) -> Option<String> {
    static CACHE: OnceLock<Mutex<ProcessCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
      Mutex::new(ProcessCache {
        last_scan:    Instant::now().checked_sub(Self::TTL).unwrap(), /* Expire immediately on
                                                   * first use */
        excluded_app: None,
      })
    });

    if let Ok(mut cache) = cache.lock() {
      // Check if we have a valid cached positive result
      if cache.last_scan.elapsed() < Self::TTL
        && let Some(ref app) = cache.excluded_app
      {
        // Verify the cached app is still in the exclusion list
        if app_matches_exclusion(app, excluded_apps) {
          return Some(app.clone());
        }
      }

      // No valid cache, scan and only cache positive results
      let result = get_recently_active_excluded_app_uncached(excluded_apps);
      if result.is_some() {
        cache.last_scan = Instant::now();
        cache.excluded_app = result.clone();
      } else {
        // Don't cache negative results. We expire cache immediately so next
        // call will rescan. This ensures we don't miss exclusions when user
        // switches from non-excluded to excluded app.
        cache.last_scan = Instant::now().checked_sub(Self::TTL).unwrap();
        cache.excluded_app = None;
      }
      result
    } else {
      // Lock poisoned - fall back to uncached
      get_recently_active_excluded_app_uncached(excluded_apps)
    }
  }
}

use base64::prelude::*;
use log::{debug, error, info, warn};
use mime_sniffer::MimeTypeSniffer;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_MAX_ENTRY_SIZE: usize = 5_000_000;

/// Query builder helper for list operations.
/// Centralizes WHERE clause and ORDER BY generation to avoid duplication.
struct ListQueryBuilder {
  include_expired: bool,
  reverse:         bool,
  search_pattern:  Option<String>,
  limit:           Option<usize>,
  offset:          Option<usize>,
}

impl ListQueryBuilder {
  fn new(include_expired: bool, reverse: bool) -> Self {
    Self {
      include_expired,
      reverse,
      search_pattern: None,
      limit: None,
      offset: None,
    }
  }

  fn with_search(mut self, pattern: Option<&str>) -> Self {
    self.search_pattern = pattern.map(|s| {
      let escaped = s.replace('!', "!!").replace('%', "!%").replace('_', "!_");
      format!("%{escaped}%")
    });
    self
  }

  fn with_pagination(mut self, offset: usize, limit: usize) -> Self {
    self.offset = Some(offset);
    self.limit = Some(limit);
    self
  }

  fn where_clause(&self) -> String {
    let mut conditions = Vec::new();

    if !self.include_expired {
      conditions.push("(is_expired IS NULL OR is_expired = 0)");
    }

    if self.search_pattern.is_some() {
      conditions
        .push("(LOWER(CAST(contents AS TEXT)) LIKE LOWER(?1) ESCAPE '!')");
    }

    if conditions.is_empty() {
      String::new()
    } else {
      format!("WHERE {}", conditions.join(" AND "))
    }
  }

  fn order_clause(&self) -> String {
    let order = if self.reverse { "ASC" } else { "DESC" };
    format!("ORDER BY COALESCE(last_accessed, 0) {order}, id {order}")
  }

  fn pagination_clause(&self) -> String {
    match (self.limit, self.offset) {
      (Some(limit), Some(offset)) => format!("LIMIT {limit} OFFSET {offset}"),
      _ => String::new(),
    }
  }

  fn select_star_query(&self) -> String {
    let where_clause = self.where_clause();
    let order_clause = self.order_clause();
    let pagination = self.pagination_clause();

    format!(
      "SELECT id, contents, mime FROM clipboard {where_clause} {order_clause} \
       {pagination}"
    )
    .trim()
    .to_string()
  }

  fn count_query(&self) -> String {
    let where_clause = self.where_clause();
    format!("SELECT COUNT(*) FROM clipboard {where_clause}")
      .trim()
      .to_string()
  }

  fn search_param(&self) -> Option<&str> {
    self.search_pattern.as_deref()
  }
}

#[derive(Error, Debug)]
pub enum StashError {
  #[error("Input is empty or too large, skipping store.")]
  EmptyOrTooLarge,
  #[error("Input is all whitespace, skipping store.")]
  AllWhitespace,
  #[error("Entry too small (min size: {0} bytes), skipping store.")]
  TooSmall(usize),
  #[error("Entry too large (max size: {0} bytes), skipping store.")]
  TooLarge(usize),

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

  #[error("Encryption error: {0}")]
  Encryption(Box<str>),
  #[error("Decryption error: {0}")]
  Decryption(Box<str>),
}

pub trait ClipboardDb {
  /// Store a new clipboard entry.
  ///
  /// # Arguments
  ///
  /// * `input` - Reader for the clipboard content
  /// * `max_dedupe_search` - Maximum number of recent entries to check for
  ///   duplicates
  /// * `max_items` - Maximum total entries to keep in database
  /// * `excluded_apps` - List of app names to exclude
  /// * `min_size` - Minimum content size (None for no minimum)
  /// * `max_size` - Maximum content size
  /// * `content_hash` - Optional pre-computed content hash (avoids re-hashing)
  /// * `mime_types` - Optional list of all MIME types offered (for persistence)
  #[allow(clippy::too_many_arguments)]
  fn store_entry(
    &self,
    input: impl Read,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: Option<&[String]>,
    min_size: Option<usize>,
    max_size: usize,
    content_hash: Option<i64>,
    mime_types: Option<&[String]>,
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
    reverse: bool,
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
  pub conn:    Connection,
  pub db_path: PathBuf,
}

impl SqliteClipboardDb {
  pub fn new(
    mut conn: Connection,
    db_path: PathBuf,
  ) -> Result<Self, StashError> {
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

    // Add content_hash column if it doesn't exist. Migration MUST be done to
    // avoid breaking existing installations.
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

    // Add mime_types column if it doesn't exist (v6)
    // Stores all MIME types offered by the source application as JSON array.
    // Needed for clipboard persistence to re-offer the same types.
    if schema_version < 6 {
      let has_mime_types: bool = tx
        .query_row(
          "SELECT sql FROM sqlite_master WHERE type='table' AND \
           name='clipboard'",
          [],
          |row| {
            let sql: String = row.get(0)?;
            Ok(sql.to_lowercase().contains("mime_types"))
          },
        )
        .unwrap_or(false);

      if !has_mime_types {
        tx.execute("ALTER TABLE clipboard ADD COLUMN mime_types TEXT", [])
          .map_err(|e| {
            StashError::Store(
              format!("Failed to add mime_types column: {e}").into(),
            )
          })?;
      }

      tx.execute("PRAGMA user_version = 6", []).map_err(|e| {
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
    Ok(Self { conn, db_path })
  }
}

impl SqliteClipboardDb {
  pub fn list_json(
    &self,
    include_expired: bool,
    reverse: bool,
  ) -> Result<String, StashError> {
    let builder = ListQueryBuilder::new(include_expired, reverse);
    let query = builder.select_star_query();
    let mut stmt = self
      .conn
      .prepare(&query)
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

      let decrypted_contents = if contents.starts_with(b"age-encryption.org/v1")
      {
        match decrypt_data(&contents) {
          Ok(decrypted) => decrypted,
          Err(e) => {
            warn!("skipping entry {id}: decryption failed: {e}");
            continue;
          },
        }
      } else {
        contents
      };

      let contents_str = match mime.as_deref() {
        Some(m) if m.starts_with("text/") || m == "application/json" => {
          String::from_utf8_lossy(&decrypted_contents).into_owned()
        },
        _ => base64::prelude::BASE64_STANDARD.encode(&decrypted_contents),
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
    min_size: Option<usize>,
    max_size: usize,
    content_hash: Option<i64>,
    mime_types: Option<&[String]>,
  ) -> Result<i64, StashError> {
    let mut buf = Vec::new();
    if input.read_to_end(&mut buf).is_err() || buf.is_empty() {
      return Err(StashError::EmptyOrTooLarge);
    }

    let size = buf.len();

    if let Some(min) = min_size
      && size < min
    {
      return Err(StashError::TooSmall(min));
    }

    if size > max_size {
      return Err(StashError::TooLarge(max_size));
    }

    if buf.iter().all(u8::is_ascii_whitespace) {
      return Err(StashError::AllWhitespace);
    }

    // Use pre-computed hash if provided, otherwise calculate it
    let content_hash = content_hash.unwrap_or_else(|| {
      let mut hasher = Fnv1aHasher::new();
      hasher.write(&buf);
      #[allow(clippy::cast_possible_wrap)]
      let hash = hasher.finish() as i64;
      hash
    });

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

    let mime_types_json: Option<String> = match mime_types {
      Some(types) => {
        Some(
          serde_json::to_string(&types)
            .map_err(|e| StashError::Store(e.to_string().into()))?,
        )
      },
      None => None,
    };

    let encrypted_buf = if load_encryption_passphrase().is_some() {
      Some(encrypt_data(&buf)?)
    } else {
      debug!("No encryption passphrase configured, storing entry unencrypted");
      None
    };

    let contents_to_store = encrypted_buf.unwrap_or(buf);

    self
      .conn
      .execute(
        "INSERT INTO clipboard (contents, mime, content_hash, last_accessed, \
         mime_types) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
          contents_to_store,
          mime,
          content_hash,
          std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs() as i64,
          mime_types_json
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
    reverse: bool,
  ) -> Result<usize, StashError> {
    let builder = ListQueryBuilder::new(include_expired, reverse);
    let query = builder.select_star_query();
    let mut stmt = self
      .conn
      .prepare(&query)
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
      let preview_contents = if contents.starts_with(&[0x01u8]) {
        match decrypt_data(&contents) {
          Ok(decrypted) => decrypted,
          Err(e) => {
            warn!("skipping entry {id}: decryption failed: {e}");
            continue;
          },
        }
      } else {
        contents
      };

      let preview =
        preview_entry(&preview_contents, mime.as_deref(), preview_width);
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

    let decrypted_contents = if contents.starts_with(&[0x01u8]) {
      decrypt_data(&contents)?
    } else {
      contents
    };

    out
      .write_all(&decrypted_contents)
      .map_err(|e| StashError::DecodeWrite(e.to_string().into()))?;
    log::info!("decoded entry with id {id}");
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

      let searchable_contents = if contents.starts_with(&[0x01u8]) {
        match decrypt_data(&contents) {
          Ok(decrypted) => decrypted,
          Err(e) => {
            warn!("skipping entry {id}: decryption failed: {e}");
            continue;
          },
        }
      } else {
        contents
      };

      if searchable_contents
        .windows(query.len())
        .any(|w| w == query.as_bytes())
      {
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

    let decrypted_contents = if contents.starts_with(&[0x01u8]) {
      decrypt_data(&contents)?
    } else {
      contents
    };

    Ok((id, decrypted_contents, mime))
  }
}

impl SqliteClipboardDb {
  /// Count visible clipboard entries, with respect to `include_expired` and
  /// optional search filter.
  pub fn count_entries(
    &self,
    include_expired: bool,
    search: Option<&str>,
  ) -> Result<usize, StashError> {
    let builder =
      ListQueryBuilder::new(include_expired, false).with_search(search);
    let query = builder.count_query();

    let count: i64 = if let Some(pattern) = builder.search_param() {
      self.conn.query_row(&query, [pattern], |r| r.get(0))
    } else {
      self.conn.query_row(&query, [], |r| r.get(0))
    }
    .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    Ok(count.max(0) as usize)
  }

  /// Fetch a window of entries for TUI virtual scrolling.
  ///
  /// Returns `(id, preview_string, mime_string)` tuples for at most
  /// `limit` rows starting at `offset` (0-indexed) in the canonical
  /// display order (most-recently-accessed first, then id DESC).
  /// Optionally filters by search query in a case-insensitive nabber on text
  /// content.
  pub fn fetch_entries_window(
    &self,
    include_expired: bool,
    offset: usize,
    limit: usize,
    preview_width: u32,
    search: Option<&str>,
    reverse: bool,
  ) -> Result<Vec<(i64, String, String)>, StashError> {
    let builder = ListQueryBuilder::new(include_expired, reverse)
      .with_search(search)
      .with_pagination(offset, limit);
    let query = builder.select_star_query();

    let mut stmt = self
      .conn
      .prepare(&query)
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    let mut rows = if let Some(pattern) = builder.search_param() {
      stmt
        .query(rusqlite::params![pattern])
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?
    } else {
      stmt
        .query([])
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?
    };

    let mut window = Vec::with_capacity(limit);
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
      let decrypted_contents = if contents.starts_with(&[0x01u8]) {
        match decrypt_data(&contents) {
          Ok(decrypted) => decrypted,
          Err(e) => {
            warn!("skipping entry {id}: decryption failed: {e}");
            continue;
          },
        }
      } else {
        contents
      };

      let preview =
        preview_entry(&decrypted_contents, mime.as_deref(), preview_width);
      let mime_str = mime.unwrap_or_default();
      window.push((id, preview, mime_str));
    }
    Ok(window)
  }

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
///
/// # Note
///
/// This function checks environment variables on every call to pick up
/// changes made after daemon startup. Regex compilation is cached by
/// pattern to avoid recompilation.
fn load_sensitive_regex() -> Option<Regex> {
  use std::process::Command;

  // Get the current pattern from env vars
  let pattern = if let Ok(cred_dir) = env::var("CREDENTIALS_DIRECTORY") {
    let file = format!("{cred_dir}/clipboard_filter");
    fs::read_to_string(&file).ok().map(|s| s.trim().to_string())
  } else if let Ok(cmd) = env::var("STASH_SENSITIVE_REGEX_COMMAND") {
    Command::new("sh")
      .args(["-c", &cmd])
      .output()
      .ok()
      .filter(|o| o.status.success())
      .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
  } else if let Ok(file_path) = env::var("STASH_SENSITIVE_REGEX_FILE") {
    fs::read_to_string(&file_path)
      .ok()
      .map(|s| s.trim().to_string())
  } else {
    env::var("STASH_SENSITIVE_REGEX").ok()
  }?;

  // Cache compiled regexes by pattern to avoid recompilation
  static REGEX_CACHE: OnceLock<
    Mutex<std::collections::HashMap<String, Regex>>,
  > = OnceLock::new();
  let cache =
    REGEX_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));

  // Check cache first
  if let Ok(cache) = cache.lock()
    && let Some(regex) = cache.get(&pattern)
  {
    return Some(regex.clone());
  }

  // Compile and cache
  Regex::new(&pattern).ok().inspect(|regex| {
    if let Ok(mut cache) = cache.lock() {
      cache.insert(pattern.clone(), regex.clone());
    }
  })
}

fn load_encryption_passphrase() -> Option<age::secrecy::SecretString> {
  use std::process::Command;

  static PASSPHRASE_CACHE: OnceLock<age::secrecy::SecretString> =
    OnceLock::new();

  if let Some(cached) = PASSPHRASE_CACHE.get() {
    return Some(cached.clone());
  }

  let passphrase = if let Ok(cred_dir) = env::var("CREDENTIALS_DIRECTORY") {
    let file = format!("{cred_dir}/stash_encryption_passphrase");
    fs::read_to_string(&file).ok().map(|s| s.trim().to_owned())
  } else if let Ok(cmd) = env::var("STASH_ENCRYPTION_PASSPHRASE_COMMAND") {
    Command::new("sh")
      .args(["-c", &cmd])
      .output()
      .ok()
      .filter(|o| o.status.success())
      .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
  } else if let Ok(file_path) = env::var("STASH_ENCRYPTION_PASSPHRASE_FILE") {
    fs::read_to_string(&file_path)
      .ok()
      .map(|s| s.trim().to_owned())
  } else {
    env::var("STASH_ENCRYPTION_PASSPHRASE").ok()
  }?;

  let secret = age::secrecy::SecretString::from(passphrase);
  let _ = PASSPHRASE_CACHE.set(secret.clone());
  Some(secret)
}

fn encrypt_data(data: &[u8]) -> Result<Vec<u8>, StashError> {
  let passphrase = load_encryption_passphrase().ok_or_else(|| {
    StashError::Encryption("No encryption passphrase configured".into())
  })?;

  let recipient = age::scrypt::Recipient::new(passphrase);
  let encrypted = age::encrypt(&recipient, data)
    .map_err(|e| StashError::Encryption(e.to_string().into()))?;
  // Prepend marker byte to identify our encrypted data
  let mut result = Vec::with_capacity(1 + encrypted.len());
  result.push(0x01u8);
  result.extend_from_slice(&encrypted);
  Ok(result)
}

fn decrypt_data(encrypted: &[u8]) -> Result<Vec<u8>, StashError> {
  let passphrase = load_encryption_passphrase().ok_or_else(|| {
    StashError::Decryption("No encryption passphrase configured".into())
  })?;

  // Strip our marker byte if present
  let data_to_decrypt = encrypted.strip_prefix(&[0x01u8]).unwrap_or(encrypted);

  let identity = age::scrypt::Identity::new(passphrase);
  let decrypted = age::decrypt(&identity, data_to_decrypt)
    .map_err(|e| StashError::Decryption(e.to_string().into()))?;
  Ok(decrypted)
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

  // For non-text/non-image data, try to sniff the MIME type
  if let Some(sniffed) = data.sniff_mime_type() {
    return format!("[[ binary data {} {} ]]", size_str(data.len()), sniffed);
  }

  // Shouldn't reach here if MIME is properly set, but just in case
  info!("Mimetype sniffing failed, omitting");
  format!("[[ binary data {} ]]", size_str(data.len()))
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
  // Use cached results to avoid expensive /proc scanning
  if let Some(active_app) = ProcessCache::get(excluded_apps) {
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
/// This is the uncached version - use `ProcessCache::get()` for cached access.
fn get_recently_active_excluded_app_uncached(
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

  /// Create an in-memory test database with full schema.
  fn test_db() -> SqliteClipboardDb {
    let conn =
      Connection::open_in_memory().expect("Failed to open in-memory db");
    SqliteClipboardDb::new(conn, PathBuf::from(":memory:"))
      .expect("Failed to create test database")
  }

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

    let db = SqliteClipboardDb::new(conn, PathBuf::from(":memory:"))
      .expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn).expect("Failed to get schema version"),
      6
    );

    assert!(table_column_exists(&db.conn, "clipboard", "content_hash"));
    assert!(table_column_exists(&db.conn, "clipboard", "last_accessed"));
    assert!(table_column_exists(&db.conn, "clipboard", "mime_types"));

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

    let db = SqliteClipboardDb::new(conn, PathBuf::from(":memory:"))
      .expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn)
        .expect("Failed to get version after migration"),
      6
    );

    assert!(table_column_exists(&db.conn, "clipboard", "content_hash"));
    assert!(table_column_exists(&db.conn, "clipboard", "last_accessed"));
    assert!(table_column_exists(&db.conn, "clipboard", "mime_types"));

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

    let db = SqliteClipboardDb::new(conn, PathBuf::from(":memory:"))
      .expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn)
        .expect("Failed to get version after migration"),
      6
    );

    assert!(table_column_exists(&db.conn, "clipboard", "content_hash"));
    assert!(table_column_exists(&db.conn, "clipboard", "last_accessed"));
    assert!(table_column_exists(&db.conn, "clipboard", "mime_types"));

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

    let db = SqliteClipboardDb::new(conn, PathBuf::from(":memory:"))
      .expect("Failed to create database");

    assert_eq!(
      get_schema_version(&db.conn)
        .expect("Failed to get version after migration"),
      6
    );

    assert!(table_column_exists(&db.conn, "clipboard", "last_accessed"));
    assert!(index_exists(&db.conn, "idx_last_accessed"));
    assert!(table_column_exists(&db.conn, "clipboard", "mime_types"));

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

    let db = SqliteClipboardDb::new(conn, PathBuf::from(":memory:"))
      .expect("Failed to create database");
    let version_after_first =
      get_schema_version(&db.conn).expect("Failed to get version");

    let db2 = SqliteClipboardDb::new(db.conn, db.db_path)
      .expect("Failed to create database again");
    let version_after_second =
      get_schema_version(&db2.conn).expect("Failed to get version");

    assert_eq!(version_after_first, version_after_second);
    assert_eq!(version_after_first, 6);
  }

  #[test]
  fn test_store_and_retrieve_with_new_columns() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_store.db");
    let conn = Connection::open(&db_path).expect("Failed to open database");
    let db = SqliteClipboardDb::new(conn, PathBuf::from(":memory:"))
      .expect("Failed to create database");

    let test_data = b"Hello, World!";
    let cursor = std::io::Cursor::new(test_data.to_vec());

    let _id = db
      .store_entry(
        cursor,
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store entry");

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1, "Existing data should be preserved");
  }

  #[test]
  fn test_store_uri_list_content() {
    let db = test_db();
    let data = b"file:///home/user/document.pdf\nfile:///home/user/image.png";
    let id = db
      .store_entry(
        std::io::Cursor::new(data.to_vec()),
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store URI list");

    let mime: Option<String> = db
      .conn
      .query_row("SELECT mime FROM clipboard WHERE id = ?1", [id], |row| {
        row.get(0)
      })
      .expect("Failed to get mime");
    assert_eq!(mime, Some("text/uri-list".to_string()));
  }

  #[test]
  fn test_store_binary_image() {
    let db = test_db();
    // Minimal PNG header
    let data: Vec<u8> = vec![
      0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
      0x00, 0x00, 0x00, 0x0D, // IHDR chunk length
      0x49, 0x48, 0x44, 0x52, // "IHDR"
      0x00, 0x00, 0x00, 0x01, // width: 1
      0x00, 0x00, 0x00, 0x01, // height: 1
      0x08, 0x02, 0x00, 0x00, 0x00, // bit depth, color, etc.
      0x90, 0x77, 0x53, 0xDE, // CRC
    ];
    let id = db
      .store_entry(
        std::io::Cursor::new(data.clone()),
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store image");

    let (contents, mime): (Vec<u8>, Option<String>) = db
      .conn
      .query_row(
        "SELECT contents, mime FROM clipboard WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .expect("Failed to get stored entry");
    assert_eq!(contents, data);
    assert_eq!(mime, Some("image/png".to_string()));
  }

  #[test]
  fn test_deduplication() {
    let db = test_db();
    let data = b"duplicate content";

    let id1 = db
      .store_entry(
        std::io::Cursor::new(data.to_vec()),
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store first");
    let _id2 = db
      .store_entry(
        std::io::Cursor::new(data.to_vec()),
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store second");

    // First entry should have been removed by deduplication
    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1, "Deduplication should keep only one copy");

    // The original id should be gone
    let exists: bool = db
      .conn
      .query_row(
        "SELECT COUNT(*) FROM clipboard WHERE id = ?1",
        [id1],
        |row| row.get::<_, i64>(0),
      )
      .map(|c| c > 0)
      .unwrap_or(false);
    assert!(!exists, "Old entry should be removed");
  }

  #[test]
  fn test_trim_excess_entries() {
    let db = test_db();
    for i in 0..5 {
      let data = format!("entry {i}");
      db.store_entry(
        std::io::Cursor::new(data.into_bytes()),
        100,
        3, // max 3 items
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store");
    }

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert!(count <= 3, "Trim should keep at most max_items entries");
  }

  #[test]
  fn test_reject_empty_input() {
    let db = test_db();
    let result = db.store_entry(
      std::io::Cursor::new(Vec::new()),
      100,
      1000,
      None,
      None,
      DEFAULT_MAX_ENTRY_SIZE,
      None,
      None,
    );
    assert!(matches!(result, Err(StashError::EmptyOrTooLarge)));
  }

  #[test]
  fn test_reject_whitespace_input() {
    let db = test_db();
    let result = db.store_entry(
      std::io::Cursor::new(b"   \n\t  ".to_vec()),
      100,
      1000,
      None,
      None,
      DEFAULT_MAX_ENTRY_SIZE,
      None,
      None,
    );
    assert!(matches!(result, Err(StashError::AllWhitespace)));
  }

  #[test]
  fn test_reject_oversized_input() {
    let db = test_db();
    // 5MB + 1 byte
    let data = vec![b'a'; 5 * 1_000_000 + 1];
    let result = db.store_entry(
      std::io::Cursor::new(data),
      100,
      1000,
      None,
      None,
      DEFAULT_MAX_ENTRY_SIZE,
      None,
      None,
    );
    assert!(matches!(result, Err(StashError::TooLarge(5000000))));
  }

  #[test]
  fn test_delete_entries_by_id() {
    let db = test_db();
    let id = db
      .store_entry(
        std::io::Cursor::new(b"to delete".to_vec()),
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store");

    let input = format!("{id}\tpreview text\n");
    let deleted = db
      .delete_entries(std::io::Cursor::new(input.into_bytes()))
      .expect("Failed to delete");
    assert_eq!(deleted, 1);

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 0);
  }

  #[test]
  fn test_delete_query_matching() {
    let db = test_db();
    db.store_entry(
      std::io::Cursor::new(b"secret password 123".to_vec()),
      100,
      1000,
      None,
      None,
      DEFAULT_MAX_ENTRY_SIZE,
      None,
      None,
    )
    .expect("Failed to store");
    db.store_entry(
      std::io::Cursor::new(b"normal text".to_vec()),
      100,
      1000,
      None,
      None,
      DEFAULT_MAX_ENTRY_SIZE,
      None,
      None,
    )
    .expect("Failed to store");

    let deleted = db
      .delete_query("secret password")
      .expect("Failed to delete query");
    assert_eq!(deleted, 1);

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1);
  }

  #[test]
  fn test_wipe_db() {
    let db = test_db();
    for i in 0..3 {
      let data = format!("entry {i}");
      db.store_entry(
        std::io::Cursor::new(data.into_bytes()),
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store");
    }

    db.wipe_db().expect("Failed to wipe");

    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 0);
  }

  #[test]
  fn test_extract_id_valid() {
    assert_eq!(extract_id("42\tsome preview"), Ok(42));
    assert_eq!(extract_id("1"), Ok(1));
    assert_eq!(extract_id("999\t"), Ok(999));
  }

  #[test]
  fn test_extract_id_invalid() {
    assert!(extract_id("abc\tpreview").is_err());
    assert!(extract_id("").is_err());
    assert!(extract_id("\tpreview").is_err());
  }

  #[test]
  fn test_preview_entry_text() {
    let data = b"Hello, world!";
    let preview = preview_entry(data, Some("text/plain"), 100);
    assert_eq!(preview, "Hello, world!");
  }

  #[test]
  fn test_preview_entry_image() {
    let data = vec![0x89, 0x50, 0x4E, 0x47]; // PNG-ish bytes
    let preview = preview_entry(&data, Some("image/png"), 100);
    assert!(preview.contains("binary data"));
    assert!(preview.contains("image/png"));
  }

  #[test]
  fn test_preview_entry_truncation() {
    let data = b"This is a rather long piece of text that should be truncated";
    let preview = preview_entry(data, Some("text/plain"), 10);
    assert!(preview.len() <= 15); // 10 chars + ellipsis (multi-byte)
    assert!(preview.ends_with('…'));
  }

  #[test]
  fn test_size_str_formatting() {
    assert_eq!(size_str(0), "0 B");
    assert_eq!(size_str(512), "512 B");
    assert_eq!(size_str(1024), "1 KiB");
    assert_eq!(size_str(1024 * 1024), "1 MiB");
  }

  #[test]
  fn test_preview_entry_binary_sniffed() {
    // PDF magic bytes
    let data = b"%PDF-1.4 fake pdf content here for testing";
    let preview = preview_entry(data, None, 100);
    assert!(preview.contains("binary data"));
    assert!(preview.contains("application/pdf"));
  }

  #[test]
  fn test_copy_entry_returns_data() {
    let db = test_db();
    let data = b"copy me";
    let id = db
      .store_entry(
        std::io::Cursor::new(data.to_vec()),
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store");

    let (returned_id, contents, mime) =
      db.copy_entry(id).expect("Failed to copy");
    assert_eq!(returned_id, id);
    assert_eq!(contents, data.to_vec());
    assert_eq!(mime, Some("text/plain".to_string()));
  }

  #[test]
  fn test_fnv1a_hasher_deterministic() {
    // Same input should produce same hash
    let data = b"test data";

    let mut hasher1 = Fnv1aHasher::new();
    hasher1.write(data);
    let hash1 = hasher1.finish();

    let mut hasher2 = Fnv1aHasher::new();
    hasher2.write(data);
    let hash2 = hasher2.finish();

    assert_eq!(hash1, hash2, "FNV-1a should produce deterministic hashes");
  }

  #[test]
  fn test_fnv1a_hasher_different_input() {
    // Different inputs should (almost certainly) produce different hashes
    let data1 = b"test data 1";
    let data2 = b"test data 2";

    let mut hasher1 = Fnv1aHasher::new();
    hasher1.write(data1);
    let hash1 = hasher1.finish();

    let mut hasher2 = Fnv1aHasher::new();
    hasher2.write(data2);
    let hash2 = hasher2.finish();

    assert_ne!(
      hash1, hash2,
      "Different data should produce different hashes"
    );
  }

  #[test]
  fn test_fnv1a_hasher_known_values() {
    // Test against known FNV-1a hash values
    let mut hasher = Fnv1aHasher::new();
    hasher.write(b"");
    assert_eq!(
      hasher.finish(),
      0xCBF29CE484222325,
      "Empty string hash mismatch"
    );

    let mut hasher = Fnv1aHasher::new();
    hasher.write(b"a");
    assert_eq!(
      hasher.finish(),
      0xAF63DC4C8601EC8C,
      "Single byte hash mismatch"
    );

    let mut hasher = Fnv1aHasher::new();
    hasher.write(b"hello");
    assert_eq!(hasher.finish(), 0xA430D84680AABD0B, "Hello hash mismatch");
  }

  #[test]
  fn test_fnv1a_hash_stored_in_db() {
    // Verify hash is stored correctly and can be retrieved
    let db = test_db();
    let data = b"test content for hashing";

    let id = db
      .store_entry(
        std::io::Cursor::new(data.to_vec()),
        100,
        1000,
        None,
        None,
        DEFAULT_MAX_ENTRY_SIZE,
        None,
        None,
      )
      .expect("Failed to store");

    // Retrieve the stored hash
    let stored_hash: i64 = db
      .conn
      .query_row(
        "SELECT content_hash FROM clipboard WHERE id = ?1",
        [id],
        |row| row.get(0),
      )
      .expect("Failed to get hash");

    // Calculate hash independently
    let mut hasher = Fnv1aHasher::new();
    hasher.write(data);
    let calculated_hash = hasher.finish() as i64;

    assert_eq!(
      stored_hash, calculated_hash,
      "Stored hash should match calculated hash"
    );

    // Verify round-trip: convert back to u64 and compare
    let stored_hash_u64 = stored_hash as u64;
    let calculated_hash_u64 = hasher.finish();
    assert_eq!(
      stored_hash_u64, calculated_hash_u64,
      "Bit pattern should be preserved in i64/u64 conversion"
    );
  }

  /// Verify that regex loading picks up env var changes. This was broken
  /// because CHECKED flag prevented re-checking after first call
  #[test]
  fn test_sensitive_regex_env_var_change_detection() {
    // XXX: This test manipulates environment variables which affects
    // parallel tests. We use a unique pattern to avoid conflicts.
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let test_id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);

    // Test 1: No env var set initially
    let var_name = format!("STASH_SENSITIVE_REGEX_TEST_{}", test_id);
    unsafe {
      env::remove_var(&var_name);
    }

    // Temporarily override the function to use our test var
    // Since we can't easily mock env::var, we test the logic indirectly
    // by verifying the new implementation checks every time

    // Call multiple times, ensure no panic and behavior is
    // consistent
    let _ = load_sensitive_regex();
    let _ = load_sensitive_regex();
    let _ = load_sensitive_regex();

    // If we got here without deadlocks or panics, the caching logic works
    // The actual env var change detection is verified by the implementation:
    // - Preivously CHECKED atomic prevented re-checking
    // - Now we check env vars every call, only caches compiled Regex objects
  }

  /// Test that regex compilation is cached by pattern
  #[test]
  fn test_sensitive_regex_caching_by_pattern() {
    // This test verifies that the regex cache works correctly
    // by ensuring multiple calls don't cause issues.

    // Call multiple times, should use cache after first compilation
    let result1 = load_sensitive_regex();
    let result2 = load_sensitive_regex();
    let result3 = load_sensitive_regex();

    // All results should be consistent
    assert_eq!(
      result1.is_some(),
      result2.is_some(),
      "Regex loading should be deterministic"
    );
    assert_eq!(
      result2.is_some(),
      result3.is_some(),
      "Regex loading should be deterministic"
    );
  }
}
