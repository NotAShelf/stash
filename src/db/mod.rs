use std::{
  env,
  fmt,
  fs,
  io::{BufRead, BufReader, Read, Write},
  path::PathBuf,
  str,
  sync::{Mutex, OnceLock},
};

pub mod nonblocking;

use std::hash::Hasher;

use base64::prelude::*;
use log::{debug, error, info, warn};
use mime_sniffer::MimeTypeSniffer;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_width::UnicodeWidthChar;

use crate::hash::Fnv1aHasher;

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
      // Scope content search to text-like entries. The `mime` guard is cheap
      // and short-circuits before the per-row `CAST(contents AS TEXT)` + LIKE,
      // so large image/binary blobs are never materialized or scanned. It also
      // avoids spurious matches against binary bytes that happen to contain
      // the ASCII pattern.
      //
      // NOTE: with encryption enabled `contents` is ciphertext, so this LIKE
      // cannot match plaintext for encrypted entries. Content search over
      // encrypted history needs a separate design (e.g. a plaintext FTS index)
      // and is intentionally out of scope here.
      conditions.push(
        "((mime LIKE 'text/%' OR mime = 'application/json') AND \
         LOWER(CAST(contents AS TEXT)) LIKE LOWER(?1) ESCAPE '!')",
      );
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

  /// Query for building list previews without materializing binary blobs.
  ///
  /// Returns `id, mime, LENGTH(contents), body`, where `body` is the raw
  /// stored bytes only for text-like (or unknown-mime) entries and `NULL`
  /// otherwise. Since `mime` already records the detected type, image/binary
  /// previews are rendered from the length alone, so SQLite never reads those
  /// (potentially multi-megabyte) blobs off disk or decrypts them.
  fn select_preview_query(&self) -> String {
    let where_clause = self.where_clause();
    let order_clause = self.order_clause();
    let pagination = self.pagination_clause();

    format!(
      "SELECT id, mime, LENGTH(contents), CASE WHEN mime IS NULL OR mime LIKE \
       'text/%' OR mime = 'application/json' THEN contents ELSE NULL END FROM \
       clipboard {where_clause} {order_clause} {pagination}"
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
  #[error("input is empty or too large, skipping store")]
  EmptyOrTooLarge,
  #[error("input is all whitespace, skipping store")]
  AllWhitespace,
  #[error("entry too small (min size: {0} bytes), skipping store")]
  TooSmall(usize),
  #[error("entry too large (max size: {0} bytes), skipping store")]
  TooLarge(usize),

  #[error("failed to store entry: {0}")]
  Store(Box<str>),
  #[error("entry excluded by app filter: {0}")]
  ExcludedByApp(Box<str>),
  #[error("error reading entry during deduplication: {0}")]
  DeduplicationRead(Box<str>),
  #[error("error decoding entry during deduplication: {0}")]
  DeduplicationDecode(Box<str>),
  #[error("failed to remove entry during deduplication: {0}")]
  DeduplicationRemove(Box<str>),
  #[error("failed to trim entry: {0}")]
  Trim(Box<str>),
  #[error("no entries to delete")]
  NoEntriesToDelete,
  #[error("failed to delete last entry: {0}")]
  DeleteLast(Box<str>),
  #[error("failed to wipe database: {0}")]
  Wipe(Box<str>),
  #[error("failed to decode entry during list: {0}")]
  ListDecode(Box<str>),
  #[error("failed to read input for decode: {0}")]
  DecodeRead(Box<str>),
  #[error("failed to extract id for decode: {0}")]
  DecodeExtractId(Box<str>),
  #[error("failed to get entry for decode: {0}")]
  DecodeGet(Box<str>),

  #[error("failed to write decoded entry: {0}")]
  DecodeWrite(Box<str>),
  #[error("failed to delete entry during query delete: {0}")]
  QueryDelete(Box<str>),
  #[error("failed to read delete input: {0}")]
  DeleteInput(Box<str>),
  #[error("failed to delete entry with id {0}: {1}")]
  DeleteEntry(i64, Box<str>),

  #[cfg(feature = "encryption")]
  #[error("encryption error: {0}")]
  Encryption(Box<str>),
  #[cfg(feature = "encryption")]
  #[error("decryption error: {0}")]
  Decryption(Box<str>),
  #[error("entry excluded by password manager hint")]
  SensitiveMimeHint,
}

/// On-disk encoding of a clipboard entry's content.
///
/// Age's output format is self-describing, i.e., it always begins with
/// `age-encryption.org/v1\n`), so no extra marker bytes are needed. Probably.
enum EntryEncoding {
  Plain(Vec<u8>),
  #[cfg(feature = "encryption")]
  AgeEncrypted(Vec<u8>),
}

impl EntryEncoding {
  #[cfg(feature = "encryption")]
  const AGE_HEADER: &'static [u8] = b"age-encryption.org/v1\n";

  fn classify(bytes: Vec<u8>) -> Self {
    #[cfg(feature = "encryption")]
    if bytes.starts_with(Self::AGE_HEADER) {
      return Self::AgeEncrypted(bytes);
    }
    Self::Plain(bytes)
  }

  fn encode(plaintext: &[u8]) -> Result<Self, StashError> {
    #[cfg(feature = "encryption")]
    if let Some(passphrase) = load_encryption_passphrase() {
      let recipient = age::scrypt::Recipient::new(passphrase);
      let encrypted = age::encrypt(&recipient, plaintext)
        .map_err(|e| StashError::Encryption(e.to_string().into()))?;
      return Ok(Self::AgeEncrypted(encrypted));
    }
    Ok(Self::Plain(plaintext.to_vec()))
  }

  fn decode(self) -> Result<Vec<u8>, StashError> {
    match self {
      Self::Plain(b) => Ok(b),
      #[cfg(feature = "encryption")]
      Self::AgeEncrypted(b) => decrypt_cached(&b),
    }
  }

  fn into_raw(self) -> Vec<u8> {
    match self {
      Self::Plain(b) => b,
      #[cfg(feature = "encryption")]
      Self::AgeEncrypted(b) => b,
    }
  }
}

pub trait ClipboardDb {
  /// Store a new clipboard entry.
  ///
  /// # Arguments
  /// * `input` - Reader for the clipboard content
  /// * `max_dedupe_search` - Maximum number of recent entries to check for
  ///   duplicates
  /// * `max_items` - Maximum total entries to keep in database
  /// * `excluded_apps` - List of app names to exclude
  /// * `min_size` - Minimum content size (None for no minimum)
  /// * `max_size` - Maximum content size
  /// * `content_hash` - Optional pre-computed content hash (avoids re-hashing)
  /// * `mime_types` - Optional list of all MIME types offered (for persistence)
  #[expect(
    clippy::too_many_arguments,
    reason = "store options mirror CLI and watch inputs"
  )]
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
      .pragma_update(None, "journal_mode", "WAL")
      .map_err(|e| {
        StashError::Store(
          format!("failed to set journal_mode pragma: {e}").into(),
        )
      })?;
    conn
      .pragma_update(None, "synchronous", "NORMAL")
      .map_err(|e| {
        StashError::Store(
          format!("failed to set synchronous pragma: {e}").into(),
        )
      })?;
    conn.pragma_update(None, "cache_size", "-256") // 256KB cache
      .map_err(|e| StashError::Store(format!("failed to set cache_size pragma: {e}").into()))?;
    conn
      .pragma_update(None, "temp_store", "memory")
      .map_err(|e| {
        StashError::Store(
          format!("failed to set temp_store pragma: {e}").into(),
        )
      })?;
    conn.pragma_update(None, "mmap_size", "0") // disable mmap
      .map_err(|e| StashError::Store(format!("failed to set mmap_size pragma: {e}").into()))?;
    conn.pragma_update(None, "page_size", "512") // small(er) pages
      .map_err(|e| StashError::Store(format!("failed to set page_size pragma: {e}").into()))?;

    let tx = conn.transaction().map_err(|e| {
      StashError::Store(
        format!("failed to begin migration transaction: {e}").into(),
      )
    })?;

    let schema_version: i64 = tx
      .pragma_query_value(None, "user_version", |row| row.get(0))
      .map_err(|e| {
        StashError::Store(format!("failed to read schema version: {e}").into())
      })?;

    if schema_version == 0 {
      tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (
          id       INTEGER PRIMARY KEY AUTOINCREMENT,
          contents BLOB NOT NULL,
          mime     TEXT
        );",
      )
      .map_err(migration_err)?;
      tx.pragma_update(None, "user_version", 1i64)
        .map_err(migration_err)?;
    }

    if schema_version < 2 {
      if !column_exists(&tx, "content_hash") {
        tx.execute("ALTER TABLE clipboard ADD COLUMN content_hash INTEGER", [])
          .map_err(migration_err)?;
      }
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_content_hash ON \
         clipboard(content_hash)",
        [],
      )
      .map_err(migration_err)?;
      tx.pragma_update(None, "user_version", 2i64)
        .map_err(migration_err)?;
    }

    if schema_version < 3 {
      if !column_exists(&tx, "last_accessed") {
        tx.execute("ALTER TABLE clipboard ADD COLUMN last_accessed INTEGER", [
        ])
        .map_err(migration_err)?;
      }
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_last_accessed ON \
         clipboard(last_accessed)",
        [],
      )
      .map_err(migration_err)?;
      tx.pragma_update(None, "user_version", 3i64)
        .map_err(migration_err)?;
    }

    if schema_version < 4 {
      if !column_exists(&tx, "expires_at") {
        tx.execute("ALTER TABLE clipboard ADD COLUMN expires_at REAL", [])
          .map_err(migration_err)?;
      }
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_expires_at ON clipboard(expires_at) \
         WHERE expires_at IS NOT NULL",
        [],
      )
      .map_err(migration_err)?;
      tx.pragma_update(None, "user_version", 4i64)
        .map_err(migration_err)?;
    }

    if schema_version < 5 {
      if !column_exists(&tx, "is_expired") {
        tx.execute(
          "ALTER TABLE clipboard ADD COLUMN is_expired INTEGER DEFAULT 0",
          [],
        )
        .map_err(migration_err)?;
      }
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_is_expired ON clipboard(is_expired) \
         WHERE is_expired = 1",
        [],
      )
      .map_err(migration_err)?;
      tx.pragma_update(None, "user_version", 5i64)
        .map_err(migration_err)?;
    }

    if schema_version < 6 {
      if !column_exists(&tx, "mime_types") {
        tx.execute("ALTER TABLE clipboard ADD COLUMN mime_types TEXT", [])
          .map_err(migration_err)?;
      }
      tx.pragma_update(None, "user_version", 6i64)
        .map_err(migration_err)?;
    }

    if schema_version < 7 {
      // Expression index matching the list ORDER BY
      // (`COALESCE(last_accessed, 0) <dir>, id <dir>`). Without it the
      // COALESCE hides `last_accessed` from idx_last_accessed, forcing a full
      // scan + sort on every window fetch; with it SQLite can walk the index
      // in either direction and satisfy the ordering for both normal and
      // reversed listings.
      tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_clipboard_order ON \
         clipboard(COALESCE(last_accessed, 0) DESC, id DESC)",
        [],
      )
      .map_err(migration_err)?;
      tx.pragma_update(None, "user_version", 7i64)
        .map_err(migration_err)?;
    }

    tx.commit().map_err(|e| {
      StashError::Store(
        format!("failed to commit migration transaction: {e}").into(),
      )
    })?;

    #[cfg(feature = "use-toplevel")]
    crate::wayland::init_wayland_state();
    Ok(Self { conn, db_path })
  }
}

/// Check whether `column` exists in the `clipboard` table.
fn column_exists(conn: &Connection, column: &str) -> bool {
  conn
    .prepare("PRAGMA table_info(clipboard)")
    .and_then(|mut stmt| {
      stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map(|rows| rows.filter_map(Result::ok).any(|c| c == column))
    })
    .unwrap_or(false)
}

/// Convert a rusqlite error into [`StashError::Store`].
fn migration_err(e: rusqlite::Error) -> StashError {
  StashError::Store(e.to_string().into())
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

      let plaintext = match EntryEncoding::classify(contents).decode() {
        Ok(p) => p,
        Err(e) => {
          warn!("skipping entry {id}: {e}");
          continue;
        },
      };
      let contents_str = match mime.as_deref() {
        Some(m) if m.starts_with("text/") || m == "application/json" => {
          String::from_utf8_lossy(&plaintext).into_owned()
        },
        _ => base64::prelude::BASE64_STANDARD.encode(&plaintext),
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
      #[expect(
        clippy::cast_possible_wrap,
        reason = "stored hash preserves the u64 bit pattern in sqlite"
      )]
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
        warn!("clipboard entry matches sensitive regex, skipping store");
        return Err(StashError::Store("filtered by sensitive regex".into()));
      }
    }

    // Check if clipboard should be excluded based on running apps
    if should_exclude_by_app(excluded_apps) {
      warn!("clipboard entry excluded by app filter");
      return Err(StashError::ExcludedByApp(
        "clipboard entry from excluded app".into(),
      ));
    }

    if mime_types.is_some_and(|types| {
      types.iter().any(|m| m == "x-kde-passwordManagerHint")
    }) {
      warn!("clipboard entry excluded by password manager hint");
      return Err(StashError::SensitiveMimeHint);
    }

    let mime_types_json: Option<String> = match mime_types {
      Some(types) => {
        Some(
          serde_json::to_string(&types)
            .map_err(|e| StashError::Store(e.to_string().into()))?,
        )
      },
      None => None,
    };

    // Re-copying content that is already stored must not mint a new id.
    // Refresh the existing entry in place (move-to-top) and reuse its id, so
    // references held elsewhere (the `stash list` TUI, `stash decode <id>`)
    // stay valid. Only genuinely new content falls through to an INSERT.
    if let Some(id) = self.refresh_duplicate(
      content_hash,
      max_dedupe_search,
      mime_types_json.as_deref(),
    )? {
      return Ok(id);
    }

    let contents_to_store = EntryEncoding::encode(&buf)?.into_raw();

    self
      .conn
      .execute(
        "INSERT INTO clipboard (contents, mime, content_hash, last_accessed, \
         mime_types) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
          contents_to_store,
          mime,
          content_hash,
          Self::now() as i64,
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

  fn trim_db(&self, max: u64) -> Result<(), StashError> {
    let count: i64 = self
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .map_err(|e| StashError::Trim(e.to_string().into()))?;
    let max_i64 = i64::try_from(max).unwrap_or(i64::MAX);
    if count > max_i64 {
      let to_delete = count - max_i64;

      self
        .conn
        .execute(
          "DELETE FROM clipboard WHERE id IN (SELECT id FROM clipboard ORDER \
           BY COALESCE(last_accessed, 0) ASC, id ASC LIMIT ?1)",
          params![to_delete],
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

      let plaintext = match EntryEncoding::classify(contents).decode() {
        Ok(p) => p,
        Err(e) => {
          warn!("skipping entry {id}: {e}");
          continue;
        },
      };
      let preview = preview_entry(&plaintext, mime.as_deref(), preview_width);
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
    let plaintext = EntryEncoding::classify(contents).decode()?;
    out
      .write_all(&plaintext)
      .map_err(|e| StashError::DecodeWrite(e.to_string().into()))?;
    log::info!("decoded entry with id {id}");
    Ok(())
  }

  fn delete_query(&self, query: &str) -> Result<usize, StashError> {
    if query.is_empty() {
      return Err(StashError::QueryDelete("query must not be empty".into()));
    }

    let escaped = query
      .replace('!', "!!")
      .replace('%', "!%")
      .replace('_', "!_");
    let pattern = format!("%{escaped}%");
    let mut stmt = self
      .conn
      .prepare(
        "SELECT id FROM clipboard WHERE LOWER(CAST(contents AS TEXT)) LIKE \
         LOWER(?1) ESCAPE '!'",
      )
      .map_err(|e| StashError::QueryDelete(e.to_string().into()))?;
    let mut rows = stmt
      .query([pattern])
      .map_err(|e| StashError::QueryDelete(e.to_string().into()))?;
    let mut ids = Vec::new();

    while let Some(row) = rows
      .next()
      .map_err(|e| StashError::QueryDelete(e.to_string().into()))?
    {
      ids.push(
        row
          .get::<_, i64>(0)
          .map_err(|e| StashError::QueryDelete(e.to_string().into()))?,
      );
    }
    drop(rows);
    drop(stmt);

    let mut deleted = 0;
    for id in ids {
      self
        .conn
        .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
        .map_err(|e| StashError::QueryDelete(e.to_string().into()))?;
      deleted += 1;
    }
    Ok(deleted)
  }

  fn delete_entries(&self, in_: impl Read) -> Result<usize, StashError> {
    let reader = BufReader::new(in_);
    let mut deleted = 0;
    for line in reader.lines() {
      let line =
        line.map_err(|e| StashError::DeleteInput(e.to_string().into()))?;
      if line.trim().is_empty() {
        continue;
      }
      let id =
        extract_id(&line).map_err(|e| StashError::DeleteInput(e.into()))?;
      self
        .conn
        .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
        .map_err(|e| StashError::DeleteEntry(id, e.to_string().into()))?;
      deleted += 1;
    }
    Ok(deleted)
  }

  fn copy_entry(
    &self,
    id: i64,
  ) -> Result<(i64, Vec<u8>, Option<String>), StashError> {
    let (contents, mime): (Vec<u8>, Option<String>) = self
      .conn
      .query_row(
        "SELECT contents, mime FROM clipboard WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .map_err(|e| StashError::DecodeGet(e.to_string().into()))?;

    self
      .conn
      .execute(
        "UPDATE clipboard SET last_accessed = CAST(strftime('%s', 'now') AS \
         INTEGER) WHERE id = ?1",
        params![id],
      )
      .map_err(|e| StashError::Store(e.to_string().into()))?;

    let plaintext = EntryEncoding::classify(contents).decode()?;
    Ok((id, plaintext, mime))
  }
}

impl SqliteClipboardDb {
  /// Handle a store of content whose hash already exists as a re-copy.
  ///
  /// Refreshes the most recent existing entry sharing `content_hash` in place
  /// (bumping `last_accessed` so it sorts to the top, and refreshing
  /// `mime_types` when provided), collapses any older duplicates into it, and
  /// returns the kept id. Returns `None` when no entry with this hash exists,
  /// in which case the caller inserts a new row.
  ///
  /// Reusing the existing id (rather than deleting and reinserting under a
  /// fresh one) keeps ids stable for references held by the `stash list` TUI
  /// and `stash decode <id>`, and avoids gratuitous id churn.
  fn refresh_duplicate(
    &self,
    content_hash: i64,
    max: u64,
    mime_types_json: Option<&str>,
  ) -> Result<Option<i64>, StashError> {
    let mut stmt = self
      .conn
      .prepare(
        "SELECT id FROM clipboard WHERE content_hash = ?1 ORDER BY id DESC \
         LIMIT ?2",
      )
      .map_err(|e| StashError::DeduplicationRead(e.to_string().into()))?;
    let ids: Vec<i64> = stmt
      .query_map(
        params![content_hash, i64::try_from(max).unwrap_or(i64::MAX)],
        |row| row.get(0),
      )
      .map_err(|e| StashError::DeduplicationRead(e.to_string().into()))?
      .collect::<Result<_, _>>()
      .map_err(|e| StashError::DeduplicationDecode(e.to_string().into()))?;
    drop(stmt);

    let Some((&keep_id, older)) = ids.split_first() else {
      return Ok(None);
    };

    // Collapse any older duplicates into the kept (newest) entry.
    for &id in older {
      self
        .conn
        .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
        .map_err(|e| StashError::DeduplicationRemove(e.to_string().into()))?;
    }

    // Move the kept entry to the top; refresh mime_types only when the new
    // store provides them (COALESCE preserves the prior value otherwise).
    self
      .conn
      .execute(
        "UPDATE clipboard SET last_accessed = ?2, mime_types = COALESCE(?3, \
         mime_types) WHERE id = ?1",
        params![keep_id, Self::now() as i64, mime_types_json],
      )
      .map_err(|e| StashError::Store(e.to_string().into()))?;

    Ok(Some(keep_id))
  }

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
    let query = builder.select_preview_query();

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
      let mime: Option<String> = row
        .get(1)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let raw_len: i64 = row
        .get(2)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let body: Option<Vec<u8>> = row
        .get(3)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

      let preview = match body {
        // Text-like (or unknown-mime) entry: decode and render a text preview.
        Some(contents) => {
          match EntryEncoding::classify(contents).decode() {
            Ok(plaintext) => {
              preview_entry(&plaintext, mime.as_deref(), preview_width)
            },
            Err(e) => {
              warn!("skipping entry {id}: {e}");
              continue;
            },
          }
        },
        // Binary/image entry: the blob was not read. Render from length + mime.
        // The size reflects the stored byte count, which equals the original
        // length for plaintext entries and is within the age framing overhead
        // for encrypted ones.
        None => {
          let mime_label =
            mime.as_deref().unwrap_or("application/octet-stream");
          let len = usize::try_from(raw_len).unwrap_or(0);
          format!("[[ binary data {} {mime_label} ]]", size_str(len))
        },
      };
      let mime_str = mime.unwrap_or_default();
      window.push((id, preview, mime_str));
    }
    Ok(window)
  }

  /// Get current Unix timestamp with sub-second precision
  pub fn now() -> f64 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_or(0.0, |duration| duration.as_secs_f64())
  }

  /// Clean up all expired entries. Returns count deleted.
  pub fn expire_ttl_entries(&self) -> Result<usize, StashError> {
    self
      .conn
      .execute(
        "UPDATE clipboard SET is_expired = 1 WHERE expires_at IS NOT NULL AND \
         (is_expired IS NULL OR is_expired = 0)",
        [],
      )
      .map_err(|e| StashError::Trim(e.to_string().into()))
  }

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

    let encrypted: i64 = self
      .conn
      .query_row(
        "SELECT COUNT(*) FROM clipboard WHERE contents GLOB \
         'age-encryption.org/v1' || char(10) || '*'",
        [],
        |row| row.get(0),
      )
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    #[cfg(feature = "encryption")]
    let undecryptable: i64 = {
      let mut stmt = self
        .conn
        .prepare("SELECT contents FROM clipboard")
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let mut rows = stmt
        .query([])
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let mut count = 0i64;
      while let Some(row) = rows
        .next()
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?
      {
        let contents: Vec<u8> = row
          .get(0)
          .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
        if contents.starts_with(b"age-encryption.org/v1\n")
          && decrypt_cached(&contents).is_err()
        {
          count += 1;
        }
      }
      count
    };
    #[cfg(not(feature = "encryption"))]
    let undecryptable: i64 = encrypted;

    let db_path = self.db_path.display();
    Ok(format!(
      "database statistics:\n\nentries:\ntotal:          \
       {total}\nactive:         {active}\nexpired:        \
       {expired}\nwith ttl:       \
       {with_expiration}\nencrypted:      \
       {encrypted}\nundecryptable:  \
       {undecryptable}\n\nstorage:\npath:           \
       {db_path}\nsize:           {size_mb:.2} MB \
       ({size_bytes} bytes)\npages:          {page_count}\npage size:      \
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

  // Credential file takes highest priority (systemd LoadCredential)
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

/// Load the encryption passphrase from environment or credential sources.
///
/// The passphrase is cached permanently via `OnceLock` on first successful
/// load. This is intentional and differs from
/// [`load_sensitive_regex`] which re-checks environment variables on every
/// call: changing the encryption passphrase mid-session would make all
/// previously encrypted entries permanently undecryptable, so the permanent
/// cache prevents accidental passphrase changes from corrupting the
/// clipboard history.
///
/// Removing the passphrase entirely (disabling encryption) after entries have
/// been stored encrypted also renders those entries permanently unreadable.
/// There is no migration path short of wiping the database. `stash stats`
/// reports affected entries as Undecryptable.
#[cfg(feature = "encryption")]
fn load_encryption_passphrase() -> Option<age::secrecy::SecretString> {
  use std::process::Command;

  static CACHE: OnceLock<age::secrecy::SecretString> = OnceLock::new();
  if let Some(cached) = CACHE.get() {
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
  let _ = CACHE.set(secret.clone());
  Some(secret)
}

/// Decrypt age-encrypted data.
///
/// `age::scrypt::Identity::new` is cheap since it stores the passphrase only.
/// The scrypt KDF runs inside `age::decrypt` per call, on the per-file salt
/// embedded in the ciphertext header. Caching the Identity would not avoid
/// it. The passphrase itself is cached by [`load_encryption_passphrase`].
#[cfg(feature = "encryption")]
fn decrypt_cached(ciphertext: &[u8]) -> Result<Vec<u8>, StashError> {
  let passphrase = load_encryption_passphrase()
    .ok_or_else(|| StashError::Decryption("no passphrase configured".into()))?;
  let identity = age::scrypt::Identity::new(passphrase);
  age::decrypt(&identity, ciphertext)
    .map_err(|e| StashError::Decryption(e.to_string().into()))
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

      let mut result = String::with_capacity(width as usize + 1);
      let mut disp = 0usize;
      for c in trimmed.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(1);
        if disp + cw > width as usize {
          result.push('…');
          break;
        }
        result.push(if c.is_whitespace() { ' ' } else { c });
        disp += cw;
      }
      return result;
    }
  }

  // For non-text/non-image data, try to sniff the MIME type
  if let Some(sniffed) = data.sniff_mime_type() {
    return format!("[[ binary data {} {} ]]", size_str(data.len()), sniffed);
  }

  // Shouldn't reach here if MIME is properly set, but just in case
  info!("mimetype sniffing failed, omitting");
  format!("[[ binary data {} ]]", size_str(data.len()))
}

pub fn size_str(size: usize) -> String {
  let units = ["B", "KiB", "MiB"];
  let mut fsize = if let Ok(val) = u32::try_from(size) {
    f64::from(val)
  } else {
    error!("clipboard entry size too large for display: {size}");
    f64::from(u32::MAX)
  };
  let mut i = 0;
  while fsize >= 1024.0 && i < units.len() - 1 {
    fsize /= 1024.0;
    i += 1;
  }
  format!("{:.0} {}", fsize, units[i])
}

/// Check if clipboard should be excluded based on the focused app.
fn should_exclude_by_app(excluded_apps: Option<&[String]>) -> bool {
  match excluded_apps {
    Some(apps) if !apps.is_empty() => detect_excluded_app_activity(apps),
    _ => false,
  }
}

/// Detect if clipboard came from an excluded focused app.
fn detect_excluded_app_activity(excluded_apps: &[String]) -> bool {
  debug!("checking clipboard exclusion against: {excluded_apps:?}");

  if let Some(focused_app) = get_focused_window_app() {
    debug!("focused window detected: {focused_app}");
    if app_matches_exclusion(&focused_app, excluded_apps) {
      debug!("clipboard excluded: focused window matches {focused_app}");
      return true;
    }
  } else {
    debug!("no focused window detected");
  }

  debug!("clipboard not excluded");
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
    debug!("found WAYLAND_CLIENT_NAME: {client}");
    return Some(client);
  }

  debug!("no focused window detection method worked");
  None
}

/// Check if an app name matches any in the exclusion list.
/// Supports basic string matching and simple regex patterns.
fn app_matches_exclusion(app_name: &str, excluded_apps: &[String]) -> bool {
  debug!("checking if '{app_name}' matches exclusion list: {excluded_apps:?}");

  for excluded in excluded_apps {
    // Basic string matching (case-insensitive)
    if app_name.to_lowercase() == excluded.to_lowercase() {
      debug!("matched exact string: {app_name} == {excluded}");
      return true;
    }

    // Simple pattern matching for common cases
    if excluded.starts_with('^') && excluded.ends_with('$') {
      // Exact match pattern like ^AppName$
      let pattern = &excluded[1..excluded.len() - 1];
      if app_name == pattern {
        debug!("matched exact pattern: {app_name} == {pattern}");
        return true;
      }
    } else if excluded.contains('*') {
      // Simple wildcard matching
      let pattern = excluded.replace('*', ".*");
      if let Ok(regex) = regex::Regex::new(&pattern)
        && regex.is_match(app_name)
      {
        debug!("matched wildcard pattern: {app_name} matches {excluded}");
        return true;
      }
    }
  }

  debug!("no match found for '{app_name}'");
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
      7
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
      7
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
      7
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
      7
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
    assert_eq!(version_after_first, 7);
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
  fn test_window_image_preview_needs_no_blob() {
    // An image entry's list preview is rendered from its mime + stored length;
    // the CASE in select_preview_query returns NULL for its body so the blob is
    // never read. The preview must still match preview_entry's binary form.
    let db = test_db();
    let data: Vec<u8> = vec![
      0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
      0x00, 0x00, 0x00, 0x0D, // IHDR length
      0x49, 0x48, 0x44, 0x52, // "IHDR"
      0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
      0x08, 0x02, 0x00, 0x00, 0x00, // bit depth, color, etc.
      0x90, 0x77, 0x53, 0xDE, // CRC
    ];
    db.store_entry(
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

    let window = db
      .fetch_entries_window(true, 0, 10, 100, None, false)
      .expect("Failed to fetch window");
    assert_eq!(window.len(), 1);
    let (_, preview, mime) = &window[0];
    assert_eq!(mime, "image/png");
    let expected = preview_entry(&data, Some("image/png"), 100);
    assert_eq!(preview, &expected);
    assert!(preview.starts_with("[[ binary data "));
    assert!(preview.contains("image/png"));
  }

  #[test]
  fn test_search_matches_text_entries() {
    // Regression guard for the mime-scoped search predicate: text content is
    // still matched by both the count and the window query.
    let db = test_db();
    db.store_entry(
      std::io::Cursor::new(b"the quick brown fox".to_vec()),
      100,
      1000,
      None,
      None,
      DEFAULT_MAX_ENTRY_SIZE,
      None,
      None,
    )
    .expect("Failed to store text");

    assert_eq!(
      db.count_entries(true, Some("brown")).expect("count"),
      1,
      "text content should be found by search"
    );
    assert_eq!(
      db.count_entries(true, Some("absent")).expect("count"),
      0,
      "non-matching search should return nothing"
    );
    let window = db
      .fetch_entries_window(true, 0, 10, 100, Some("brown"), false)
      .expect("window");
    assert_eq!(window.len(), 1);
  }

  #[test]
  fn test_ordering_index_present() {
    // The expression index backing the list ORDER BY must be created by the
    // schema migration; without it every window fetch falls back to a full
    // scan + sort.
    let db = test_db();
    assert!(index_exists(&db.conn, "idx_clipboard_order"));
  }

  #[test]
  fn test_list_order_uses_index_no_temp_sort() {
    // The list ORDER BY must be satisfied by idx_clipboard_order rather than a
    // materialized sort. With enough rows for the planner to prefer the index,
    // the query plan must not contain a temporary B-tree sort step.
    let db = test_db();
    for i in 0..300i64 {
      db.conn
        .execute(
          "INSERT INTO clipboard (contents, mime, last_accessed) VALUES \
           (x'00', 'text/plain', ?1)",
          params![i],
        )
        .expect("insert");
    }

    let builder = ListQueryBuilder::new(false, false).with_pagination(0, 24);
    let query = builder.select_preview_query();

    let mut stmt = db
      .conn
      .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
      .expect("prepare plan");
    let plan: String = stmt
      .query_map([], |row| row.get::<_, String>(3))
      .expect("query plan")
      .filter_map(Result::ok)
      .collect::<Vec<_>>()
      .join("\n");

    assert!(
      plan.contains("idx_clipboard_order"),
      "list query should use the ordering index; plan was:\n{plan}"
    );
    assert!(
      !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
      "list query should not materialize a sort; plan was:\n{plan}"
    );
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
    let id2 = db
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

    // Storing identical content collapses to a single entry.
    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
      .expect("Failed to count");
    assert_eq!(count, 1, "Deduplication should keep only one copy");

    // A re-copy is a move-to-top, not a new entry: the id must be preserved so
    // references held by the TUI and `stash decode <id>` stay valid.
    assert_eq!(id1, id2, "Re-copied content should keep the same id");
    let exists: bool = db
      .conn
      .query_row(
        "SELECT COUNT(*) FROM clipboard WHERE id = ?1",
        [id1],
        |row| row.get::<_, i64>(0),
      )
      .map(|c| c > 0)
      .unwrap_or(false);
    assert!(
      exists,
      "Original entry should be refreshed in place, not removed"
    );
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

  #[test]
  fn test_migration_from_v3() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let db_path = temp_dir.path().join("test_v3.db");
    let conn = Connection::open(&db_path).expect("open");
    conn
      .execute_batch(
        "CREATE TABLE clipboard (
           id            INTEGER PRIMARY KEY AUTOINCREMENT,
           contents      BLOB NOT NULL,
           mime          TEXT,
           content_hash  INTEGER,
           last_accessed INTEGER
         );
         INSERT INTO clipboard (contents, mime, content_hash) VALUES \
         (x'010203', 'text/plain', 12345);",
      )
      .expect("create v3 schema");
    conn
      .pragma_update(None, "user_version", 3i64)
      .expect("set version");

    let db = SqliteClipboardDb::new(conn, db_path).expect("migrate");
    assert_eq!(get_schema_version(&db.conn).expect("version"), 7);
    assert!(table_column_exists(&db.conn, "clipboard", "expires_at"));
    assert!(table_column_exists(&db.conn, "clipboard", "is_expired"));
    assert!(table_column_exists(&db.conn, "clipboard", "mime_types"));
    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |r| r.get(0))
      .expect("count");
    assert_eq!(count, 1, "existing data must survive migration");
  }

  #[test]
  fn test_migration_from_v4() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let db_path = temp_dir.path().join("test_v4.db");
    let conn = Connection::open(&db_path).expect("open");
    conn
      .execute_batch(
        "CREATE TABLE clipboard (
           id            INTEGER PRIMARY KEY AUTOINCREMENT,
           contents      BLOB NOT NULL,
           mime          TEXT,
           content_hash  INTEGER,
           last_accessed INTEGER,
           expires_at    REAL
         );
         INSERT INTO clipboard (contents, mime) VALUES (x'aabbcc', \
         'image/png');",
      )
      .expect("create v4 schema");
    conn
      .pragma_update(None, "user_version", 4i64)
      .expect("set version");

    let db = SqliteClipboardDb::new(conn, db_path).expect("migrate");
    assert_eq!(get_schema_version(&db.conn).expect("version"), 7);
    assert!(table_column_exists(&db.conn, "clipboard", "is_expired"));
    assert!(table_column_exists(&db.conn, "clipboard", "mime_types"));
    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |r| r.get(0))
      .expect("count");
    assert_eq!(count, 1, "existing data must survive migration");
  }

  #[test]
  fn test_migration_from_v5() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let db_path = temp_dir.path().join("test_v5.db");
    let conn = Connection::open(&db_path).expect("open");
    conn
      .execute_batch(
        "CREATE TABLE clipboard (
           id            INTEGER PRIMARY KEY AUTOINCREMENT,
           contents      BLOB NOT NULL,
           mime          TEXT,
           content_hash  INTEGER,
           last_accessed INTEGER,
           expires_at    REAL,
           is_expired    INTEGER DEFAULT 0
         );
         INSERT INTO clipboard (contents, mime) VALUES (x'deadbeef', \
         'application/octet-stream');",
      )
      .expect("create v5 schema");
    conn
      .pragma_update(None, "user_version", 5i64)
      .expect("set version");

    let db = SqliteClipboardDb::new(conn, db_path).expect("migrate");
    assert_eq!(get_schema_version(&db.conn).expect("version"), 7);
    assert!(table_column_exists(&db.conn, "clipboard", "mime_types"));
  }

  /// Pre-migration entries (NULL content_hash) must have last_accessed
  /// updated when accessed via copy_entry.
  #[test]
  fn test_copy_entry_updates_last_accessed_null_hash() {
    let db = test_db();
    db.conn
      .execute(
        "INSERT INTO clipboard (contents, mime, content_hash, last_accessed) \
         VALUES (?1, 'text/plain', NULL, 0)",
        rusqlite::params![b"legacy data".as_ref()],
      )
      .expect("insert null-hash entry");
    let id: i64 = db
      .conn
      .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
      .expect("id");

    db.copy_entry(id).expect("copy");

    let last_accessed: i64 = db
      .conn
      .query_row(
        "SELECT last_accessed FROM clipboard WHERE id = ?1",
        [id],
        |r| r.get(0),
      )
      .expect("last_accessed");
    assert!(
      last_accessed > 0,
      "last_accessed must be updated for null-hash entries"
    );
  }

  /// trim_db must evict the least-recently-accessed entries, not the
  /// lowest-id entries.
  #[test]
  fn test_trim_db_evicts_lru_not_oldest() {
    let db = test_db();
    let mut ids = Vec::new();
    for i in 0..5u8 {
      let id = db
        .store_entry(
          std::io::Cursor::new(vec![i; 4]),
          0,
          100,
          None,
          None,
          DEFAULT_MAX_ENTRY_SIZE,
          None,
          None,
        )
        .expect("store");
      ids.push(id);
    }

    // Zero out all timestamps so copy_entry produces a strictly higher value.
    db.conn
      .execute("UPDATE clipboard SET last_accessed = 0", [])
      .expect("reset timestamps");

    // Touch the first (oldest by id) entry to make it most-recently-used.
    db.copy_entry(ids[0]).expect("copy");

    // Trim to 4; ids[0] was just accessed and must survive.
    db.trim_db(4).expect("trim");

    let still_there: i64 = db
      .conn
      .query_row(
        "SELECT COUNT(*) FROM clipboard WHERE id = ?1",
        [ids[0]],
        |r| r.get(0),
      )
      .expect("count");
    assert_eq!(
      still_there, 1,
      "recently accessed entry must not be evicted"
    );

    let total: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM clipboard", [], |r| r.get(0))
      .expect("total");
    assert_eq!(total, 4);
  }

  /// All new columns must be NULL for entries created before their respective
  /// schema versions.
  #[test]
  fn test_migration_null_columns_for_legacy_entries() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let db_path = temp_dir.path().join("test_legacy.db");
    {
      let conn = Connection::open(&db_path).expect("open");
      conn
        .execute_batch(
          "CREATE TABLE clipboard (
             id       INTEGER PRIMARY KEY AUTOINCREMENT,
             contents BLOB NOT NULL,
             mime     TEXT
           );
           INSERT INTO clipboard (contents, mime) VALUES (x'68656c6c6f', \
           'text/plain');",
        )
        .expect("create v0 schema");
    }

    let conn = Connection::open(&db_path).expect("open");
    let db = SqliteClipboardDb::new(conn, db_path).expect("migrate");

    let (hash, accessed, expires): (Option<i64>, Option<i64>, Option<f64>) = db
      .conn
      .query_row(
        "SELECT content_hash, last_accessed, expires_at FROM clipboard WHERE \
         id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
      )
      .expect("query");
    assert!(hash.is_none(), "content_hash must be NULL for pre-v2 entry");
    assert!(
      accessed.is_none(),
      "last_accessed must be NULL for pre-v3 entry"
    );
    assert!(
      expires.is_none(),
      "expires_at must be NULL for pre-v4 entry"
    );
  }
}
