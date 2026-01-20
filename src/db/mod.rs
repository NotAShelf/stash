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
use imagesize::ImageType;
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
  fn next_sequence(&self) -> i64;
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
  pub fn new(conn: Connection) -> Result<Self, StashError> {
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

    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                contents BLOB NOT NULL,
                mime TEXT
            );",
      )
      .map_err(|e| StashError::Store(e.to_string().into()))?;

    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                contents BLOB NOT NULL,
                mime TEXT,
                content_hash INTEGER,
                last_accessed INTEGER DEFAULT (CAST(strftime('%s', 'now') AS \
         INTEGER))
            );",
      )
      .map_err(|e| StashError::Store(e.to_string().into()))?;

    // Add content_hash column if it doesn't exist
    // Migration MUST be done to avoid breaking existing installations.
    let _ =
      conn.execute("ALTER TABLE clipboard ADD COLUMN content_hash INTEGER", []);

    // Add last_accessed column if it doesn't exist
    let _ = conn.execute(
      "ALTER TABLE clipboard ADD COLUMN last_accessed INTEGER DEFAULT \
       (CAST(strftime('%s', 'now') AS INTEGER))",
      [],
    );

    // Create index for content_hash if it doesn't exist
    let _ = conn.execute(
      "CREATE INDEX IF NOT EXISTS idx_content_hash ON clipboard(content_hash)",
      [],
    );

    // Create index for last_accessed if it doesn't exist
    let _ = conn.execute(
      "CREATE INDEX IF NOT EXISTS idx_last_accessed ON \
       clipboard(last_accessed)",
      [],
    );

    // Initialize Wayland state in background thread. This will be used to track
    // focused window state.
    #[cfg(feature = "use-toplevel")]
    crate::wayland::init_wayland_state();
    Ok(Self { conn })
  }
}

impl SqliteClipboardDb {
  pub fn list_json(&self) -> Result<String, StashError> {
    let mut stmt = self
      .conn
      .prepare(
        "SELECT id, contents, mime FROM clipboard ORDER BY \
         COALESCE(last_accessed, 0) DESC, id DESC",
      )
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

    let mime = detect_mime_optimized(&buf);

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
        "INSERT INTO clipboard (contents, mime, content_hash) VALUES (?1, ?2, \
         ?3)",
        params![buf, mime, content_hash],
      )
      .map_err(|e| StashError::Store(e.to_string().into()))?;

    self.trim_db(max_items)?;
    Ok(self.next_sequence())
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
  ) -> Result<usize, StashError> {
    let mut stmt = self
      .conn
      .prepare(
        "SELECT id, contents, mime FROM clipboard ORDER BY \
         COALESCE(last_accessed, 0) DESC, id DESC",
      )
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

  fn next_sequence(&self) -> i64 {
    match self
      .conn
      .query_row("SELECT MAX(id) FROM clipboard", [], |row| {
        row.get::<_, Option<i64>>(0)
      }) {
      Ok(Some(max_id)) => max_id + 1,
      Ok(None) | Err(_) => 1,
    }
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

pub fn detect_mime_optimized(data: &[u8]) -> Option<String> {
  // Check if it's valid UTF-8 first, which most clipboard content are.
  // This will be used to return early without unnecessary mimetype detection
  // overhead.
  if std::str::from_utf8(data).is_ok() {
    return Some("text/plain".to_string());
  }

  // Only run image detection on binary data
  detect_mime(data)
}

pub fn detect_mime(data: &[u8]) -> Option<String> {
  if let Ok(img_type) = imagesize::image_type(data) {
    let mime_str = match img_type {
      ImageType::Png => "image/png",
      ImageType::Jpeg => "image/jpeg",
      ImageType::Gif => "image/gif",
      ImageType::Bmp => "image/bmp",
      ImageType::Tiff => "image/tiff",
      ImageType::Webp => "image/webp",
      ImageType::Aseprite => "image/x-aseprite",
      ImageType::Dds => "image/vnd.ms-dds",
      ImageType::Exr => "image/aces",
      ImageType::Farbfeld => "image/farbfeld",
      ImageType::Hdr => "image/vnd.radiance",
      ImageType::Ico => "image/x-icon",
      ImageType::Ilbm => "image/ilbm",
      ImageType::Jxl => "image/jxl",
      ImageType::Ktx2 => "image/ktx2",
      ImageType::Pnm => "image/x-portable-anymap",
      ImageType::Psd => "image/vnd.adobe.photoshop",
      ImageType::Qoi => "image/qoi",
      ImageType::Tga => "image/x-tga",
      ImageType::Vtf => "image/x-vtf",
      ImageType::Heif(_) => "image/heif",
      _ => "application/octet-stream",
    };
    Some(mime_str.to_string())
  } else {
    None
  }
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
