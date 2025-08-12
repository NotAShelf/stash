use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::str;

use imagesize::{ImageSize, ImageType};
use log::{error, info};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;

#[derive(Error, Debug)]
pub enum StashError {
    #[error("Input is empty or too large, skipping store.")]
    EmptyOrTooLarge,
    #[error("Input is all whitespace, skipping store.")]
    AllWhitespace,

    #[error("Failed to store entry: {0}")]
    Store(String),
    #[error("Error reading entry during deduplication: {0}")]
    DeduplicationRead(String),
    #[error("Error decoding entry during deduplication: {0}")]
    DeduplicationDecode(String),
    #[error("Failed to remove entry during deduplication: {0}")]
    DeduplicationRemove(String),
    #[error("Failed to trim entry: {0}")]
    Trim(String),
    #[error("No entries to delete")]
    NoEntriesToDelete,
    #[error("Failed to delete last entry: {0}")]
    DeleteLast(String),
    #[error("Failed to wipe database: {0}")]
    Wipe(String),
    #[error("Failed to decode entry during list: {0}")]
    ListDecode(String),
    #[error("Failed to read input for decode: {0}")]
    DecodeRead(String),
    #[error("Failed to extract id for decode: {0}")]
    DecodeExtractId(String),
    #[error("Failed to get entry for decode: {0}")]
    DecodeGet(String),

    #[error("Failed to write decoded entry: {0}")]
    DecodeWrite(String),
    #[error("Failed to delete entry during query delete: {0}")]
    QueryDelete(String),
    #[error("Failed to delete entry with id {0}: {1}")]
    DeleteEntry(u64, String),
}

pub trait ClipboardDb {
    fn store_entry(
        &self,
        input: impl Read,
        max_dedupe_search: u64,
        max_items: u64,
    ) -> Result<u64, StashError>;
    fn deduplicate(&self, buf: &[u8], max: u64) -> Result<usize, StashError>;
    fn trim_db(&self, max: u64) -> Result<(), StashError>;
    fn delete_last(&self) -> Result<(), StashError>;
    fn wipe_db(&self) -> Result<(), StashError>;
    fn list_entries(&self, out: impl Write, preview_width: u32) -> Result<usize, StashError>;
    fn decode_entry(
        &self,
        in_: impl Read,
        out: impl Write,
        input: Option<String>,
    ) -> Result<(), StashError>;
    fn delete_query(&self, query: &str) -> Result<usize, StashError>;
    fn delete_entries(&self, in_: impl Read) -> Result<usize, StashError>;
    fn next_sequence(&self) -> u64;
}

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub contents: Vec<u8>,
    pub mime: Option<String>,
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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS clipboard (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                contents BLOB NOT NULL,
                mime TEXT
            );",
        )
        .map_err(|e| StashError::Store(e.to_string()))?;
        Ok(Self { conn })
    }
}

impl SqliteClipboardDb {
    pub fn list_json(&self) -> Result<String, StashError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, contents, mime FROM clipboard ORDER BY id DESC")
            .map_err(|e| StashError::ListDecode(e.to_string()))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| StashError::ListDecode(e.to_string()))?;

        let mut entries = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| StashError::ListDecode(e.to_string()))?
        {
            let id: u64 = row
                .get(0)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let contents: Vec<u8> = row
                .get(1)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let mime: Option<String> = row
                .get(2)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let contents_str = match mime.as_deref() {
                Some(m) if m.starts_with("text/") || m == "application/json" => {
                    String::from_utf8_lossy(&contents).to_string()
                }
                _ => STANDARD.encode(&contents),
            };
            entries.push(json!({
                "id": id,
                "contents": contents_str,
                "mime": mime,
            }));
        }

        Ok(serde_json::to_string_pretty(&entries)
            .map_err(|e| StashError::ListDecode(e.to_string()))?)
    }
}

impl ClipboardDb for SqliteClipboardDb {
    fn store_entry(
        &self,
        mut input: impl Read,
        max_dedupe_search: u64,
        max_items: u64,
    ) -> Result<u64, StashError> {
        let mut buf = Vec::new();
        if input.read_to_end(&mut buf).is_err() || buf.is_empty() || buf.len() > 5 * 1_000_000 {
            return Err(StashError::EmptyOrTooLarge);
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            return Err(StashError::AllWhitespace);
        }

        let mime = detect_mime(&buf);

        self.deduplicate(&buf, max_dedupe_search)?;

        self.conn
            .execute(
                "INSERT INTO clipboard (contents, mime) VALUES (?1, ?2)",
                params![buf, mime],
            )
            .map_err(|e| StashError::Store(e.to_string()))?;

        self.trim_db(max_items)?;
        Ok(self.next_sequence())
    }

    fn deduplicate(&self, buf: &[u8], max: u64) -> Result<usize, StashError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, contents FROM clipboard ORDER BY id DESC LIMIT ?1")
            .map_err(|e| StashError::DeduplicationRead(e.to_string()))?;
        let mut rows = stmt
            .query(params![i64::try_from(max).unwrap_or(i64::MAX)])
            .map_err(|e| StashError::DeduplicationRead(e.to_string()))?;
        let mut deduped = 0;
        while let Some(row) = rows
            .next()
            .map_err(|e| StashError::DeduplicationRead(e.to_string()))?
        {
            let id: u64 = row
                .get(0)
                .map_err(|e| StashError::DeduplicationDecode(e.to_string()))?;
            let contents: Vec<u8> = row
                .get(1)
                .map_err(|e| StashError::DeduplicationDecode(e.to_string()))?;
            if contents == buf {
                self.conn
                    .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
                    .map_err(|e| StashError::DeduplicationRemove(e.to_string()))?;
                deduped += 1;
            }
        }
        Ok(deduped)
    }

    fn trim_db(&self, max: u64) -> Result<(), StashError> {
        let count: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM clipboard", [], |row| row.get(0))
            .map_err(|e| StashError::Trim(e.to_string()))?;
        if count > max {
            let to_delete = count - max;
            self.conn.execute(
                "DELETE FROM clipboard WHERE id IN (SELECT id FROM clipboard ORDER BY id ASC LIMIT ?1)",
                params![i64::try_from(to_delete).unwrap_or(i64::MAX)],
            ).map_err(|e| StashError::Trim(e.to_string()))?;
        }
        Ok(())
    }

    fn delete_last(&self) -> Result<(), StashError> {
        let id: Option<u64> = self
            .conn
            .query_row(
                "SELECT id FROM clipboard ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| StashError::DeleteLast(e.to_string()))?;
        if let Some(id) = id {
            self.conn
                .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
                .map_err(|e| StashError::DeleteLast(e.to_string()))?;
            Ok(())
        } else {
            Err(StashError::NoEntriesToDelete)
        }
    }

    fn wipe_db(&self) -> Result<(), StashError> {
        self.conn
            .execute("DELETE FROM clipboard", [])
            .map_err(|e| StashError::Wipe(e.to_string()))?;
        Ok(())
    }

    fn list_entries(&self, mut out: impl Write, preview_width: u32) -> Result<usize, StashError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, contents, mime FROM clipboard ORDER BY id DESC")
            .map_err(|e| StashError::ListDecode(e.to_string()))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| StashError::ListDecode(e.to_string()))?;
        let mut listed = 0;
        while let Some(row) = rows
            .next()
            .map_err(|e| StashError::ListDecode(e.to_string()))?
        {
            let id: u64 = row
                .get(0)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let contents: Vec<u8> = row
                .get(1)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let mime: Option<String> = row
                .get(2)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let preview = preview_entry(&contents, mime.as_deref(), preview_width);
            if writeln!(out, "{id}\t{preview}").is_ok() {
                listed += 1;
            }
        }
        Ok(listed)
    }

    fn decode_entry(
        &self,
        mut in_: impl Read,
        mut out: impl Write,
        input: Option<String>,
    ) -> Result<(), StashError> {
        let s = if let Some(input) = input {
            input
        } else {
            let mut buf = String::new();
            in_.read_to_string(&mut buf)
                .map_err(|e| StashError::DecodeRead(e.to_string()))?;
            buf
        };
        let id = extract_id(&s).map_err(|e| StashError::DecodeExtractId(e.to_string()))?;
        let (contents, _mime): (Vec<u8>, Option<String>) = self
            .conn
            .query_row(
                "SELECT contents, mime FROM clipboard WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| StashError::DecodeGet(e.to_string()))?;
        out.write_all(&contents)
            .map_err(|e| StashError::DecodeWrite(e.to_string()))?;
        info!("Decoded entry with id {id}");
        Ok(())
    }

    fn delete_query(&self, query: &str) -> Result<usize, StashError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, contents FROM clipboard")
            .map_err(|e| StashError::QueryDelete(e.to_string()))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| StashError::QueryDelete(e.to_string()))?;
        let mut deleted = 0;
        while let Some(row) = rows
            .next()
            .map_err(|e| StashError::QueryDelete(e.to_string()))?
        {
            let id: u64 = row
                .get(0)
                .map_err(|e| StashError::QueryDelete(e.to_string()))?;
            let contents: Vec<u8> = row
                .get(1)
                .map_err(|e| StashError::QueryDelete(e.to_string()))?;
            if contents.windows(query.len()).any(|w| w == query.as_bytes()) {
                self.conn
                    .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
                    .map_err(|e| StashError::QueryDelete(e.to_string()))?;
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
                self.conn
                    .execute("DELETE FROM clipboard WHERE id = ?1", params![id])
                    .map_err(|e| StashError::DeleteEntry(id, e.to_string()))?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    fn next_sequence(&self) -> u64 {
        match self
            .conn
            .query_row("SELECT MAX(id) FROM clipboard", [], |row| {
                row.get::<_, Option<u64>>(0)
            }) {
            Ok(Some(max_id)) => max_id + 1,
            Ok(None) | Err(_) => 1,
        }
    }
}

// Helper functions
pub fn extract_id(input: &str) -> Result<u64, &'static str> {
    let id_str = input.split('\t').next().unwrap_or("");
    id_str.parse().map_err(|_| "invalid id")
}

pub fn detect_mime(data: &[u8]) -> Option<String> {
    if let Ok(img_type) = imagesize::image_type(data) {
        Some(
            match img_type {
                ImageType::Png => "image/png",
                ImageType::Jpeg => "image/jpeg",
                ImageType::Gif => "image/gif",
                ImageType::Bmp => "image/bmp",
                ImageType::Tiff => "image/tiff",
                ImageType::Webp => "image/webp",
                _ => "application/octet-stream",
            }
            .to_string(),
        )
    } else {
        None
    }
}

pub fn preview_entry(data: &[u8], mime: Option<&str>, width: u32) -> String {
    if let Some(mime) = mime {
        if mime.starts_with("image/") {
            if let Ok(ImageSize {
                width: img_width,
                height: img_height,
            }) = imagesize::blob_size(data)
            {
                return format!(
                    "[[ binary data {} {} {}x{} ]]",
                    size_str(data.len()),
                    mime,
                    img_width,
                    img_height
                );
            }
        } else if mime == "application/json" || mime.starts_with("text/") {
            let s = match str::from_utf8(data) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to decode UTF-8 clipboard data: {e}");
                    ""
                }
            };
            let s = s.trim().replace(|c: char| c.is_whitespace(), " ");
            return truncate(&s, width as usize, "…");
        }
    }
    let s = String::from_utf8_lossy(data);
    truncate(s.trim(), width as usize, "…")
}

pub fn truncate(s: &str, max: usize, ellip: &str) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect::<String>() + ellip
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
