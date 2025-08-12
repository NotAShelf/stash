use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::str;

use image::{GenericImageView, ImageFormat};
use log::{error, info};
use rmp_serde::{decode::from_read, encode::to_vec};
use serde::{Deserialize, Serialize};
use sled::{Db, IVec};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StashError {
    #[error("Input is empty or too large, skipping store.")]
    EmptyOrTooLarge,
    #[error("Input is all whitespace, skipping store.")]
    AllWhitespace,
    #[error("Failed to serialize entry: {0}")]
    Serialize(String),
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
    #[error("No entry found for id {0}")]
    DecodeNoEntry(u64),
    #[error("Failed to decode entry: {0}")]
    DecodeDecode(String),
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

pub struct SledClipboardDb {
    pub db: Db,
}

impl ClipboardDb for SledClipboardDb {
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

        let entry = Entry {
            contents: buf.clone(),
            mime,
        };

        let id = self.next_sequence();
        let enc = to_vec(&entry).map_err(|e| StashError::Serialize(e.to_string()))?;

        self.db
            .insert(u64_to_ivec(id), enc)
            .map_err(|e| StashError::Store(e.to_string()))?;
        self.trim_db(max_items)?;
        Ok(id)
    }

    fn deduplicate(&self, buf: &[u8], max: u64) -> Result<usize, StashError> {
        let mut count = 0;
        let mut deduped = 0;
        for item in self
            .db
            .iter()
            .rev()
            .take(usize::try_from(max).unwrap_or(usize::MAX))
        {
            let (k, v) = match item {
                Ok((k, v)) => (k, v),
                Err(e) => return Err(StashError::DeduplicationRead(e.to_string())),
            };
            let entry: Entry = match from_read(v.as_ref()) {
                Ok(e) => e,
                Err(e) => return Err(StashError::DeduplicationDecode(e.to_string())),
            };
            if entry.contents == buf {
                self.db
                    .remove(k)
                    .map(|_| {
                        deduped += 1;
                    })
                    .map_err(|e| StashError::DeduplicationRemove(e.to_string()))?;
            }
            count += 1;
            if count >= max {
                break;
            }
        }
        Ok(deduped)
    }

    fn trim_db(&self, max: u64) -> Result<(), StashError> {
        let mut keys: Vec<_> = self
            .db
            .iter()
            .rev()
            .filter_map(|kv| match kv {
                Ok((k, _)) => Some(k),
                Err(_e) => None,
            })
            .collect();
        if keys.len() as u64 > max {
            for k in keys.drain(usize::try_from(max).unwrap_or(0)..) {
                self.db
                    .remove(k)
                    .map_err(|e| StashError::Trim(e.to_string()))?;
            }
        }
        Ok(())
    }

    fn delete_last(&self) -> Result<(), StashError> {
        if let Some((k, _)) = self.db.iter().next_back().and_then(Result::ok) {
            self.db
                .remove(k)
                .map(|_| ())
                .map_err(|e| StashError::DeleteLast(e.to_string()))
        } else {
            Err(StashError::NoEntriesToDelete)
        }
    }

    fn wipe_db(&self) -> Result<(), StashError> {
        self.db.clear().map_err(|e| StashError::Wipe(e.to_string()))
    }

    fn list_entries(&self, mut out: impl Write, preview_width: u32) -> Result<usize, StashError> {
        let mut listed = 0;
        for (k, v) in self.db.iter().rev().filter_map(Result::ok) {
            let id = ivec_to_u64(&k);
            let entry: Entry = match from_read(v.as_ref()) {
                Ok(e) => e,
                Err(e) => return Err(StashError::ListDecode(e.to_string())),
            };
            let preview = preview_entry(&entry.contents, entry.mime.as_deref(), preview_width);
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
        let v = self
            .db
            .get(u64_to_ivec(id))
            .map_err(|e| StashError::DecodeGet(e.to_string()))?
            .ok_or(StashError::DecodeNoEntry(id))?;
        let entry: Entry =
            from_read(v.as_ref()).map_err(|e| StashError::DecodeDecode(e.to_string()))?;

        out.write_all(&entry.contents)
            .map_err(|e| StashError::DecodeWrite(e.to_string()))?;
        info!("Decoded entry with id {id}");
        Ok(())
    }

    fn delete_query(&self, query: &str) -> Result<usize, StashError> {
        let mut deleted = 0;
        for (k, v) in self.db.iter().filter_map(Result::ok) {
            let entry: Entry = match from_read(v.as_ref()) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry
                .contents
                .windows(query.len())
                .any(|w| w == query.as_bytes())
            {
                self.db
                    .remove(k)
                    .map(|_| {
                        deleted += 1;
                    })
                    .map_err(|e| StashError::QueryDelete(e.to_string()))?;
            }
        }
        Ok(deleted)
    }

    fn delete_entries(&self, in_: impl Read) -> Result<usize, StashError> {
        let reader = BufReader::new(in_);
        let mut deleted = 0;
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(id) = extract_id(&line) {
                self.db
                    .remove(u64_to_ivec(id))
                    .map(|_| {
                        deleted += 1;
                    })
                    .map_err(|e| StashError::DeleteEntry(id, e.to_string()))?;
            }
        }
        Ok(deleted)
    }

    fn next_sequence(&self) -> u64 {
        let last = self
            .db
            .iter()
            .next_back()
            .and_then(std::result::Result::ok)
            .map(|(k, _)| ivec_to_u64(&k));
        last.unwrap_or(0) + 1
    }
}

// Helper functions
pub fn extract_id(input: &str) -> Result<u64, &'static str> {
    let id_str = input.split('\t').next().unwrap_or("");
    id_str.parse().map_err(|_| "invalid id")
}

pub fn u64_to_ivec(v: u64) -> IVec {
    IVec::from(&v.to_be_bytes()[..])
}

pub fn ivec_to_u64(v: &IVec) -> u64 {
    let arr: [u8; 8] = if let Ok(arr) = v.as_ref().try_into() {
        arr
    } else {
        error!("Failed to convert IVec to u64: invalid length");
        return 0;
    };
    u64::from_be_bytes(arr)
}

pub fn detect_mime(data: &[u8]) -> Option<String> {
    if image::guess_format(data).is_ok() {
        match image::guess_format(data) {
            Ok(fmt) => Some(
                match fmt {
                    ImageFormat::Png => "image/png",
                    ImageFormat::Jpeg => "image/jpeg",
                    ImageFormat::Gif => "image/gif",
                    ImageFormat::Bmp => "image/bmp",
                    ImageFormat::Tiff => "image/tiff",
                    _ => "application/octet-stream",
                }
                .to_string(),
            ),
            Err(_) => None,
        }
    } else if data.is_ascii() {
        Some("text/plain".into())
    } else {
        None
    }
}

pub fn preview_entry(data: &[u8], mime: Option<&str>, width: u32) -> String {
    if let Some(mime) = mime {
        if mime.starts_with("image/") {
            if let Ok(img) = image::load_from_memory(data) {
                let (w, h) = img.dimensions();
                return format!(
                    "[[ binary data {} {} {}x{} ]]",
                    size_str(data.len()),
                    mime,
                    w,
                    h
                );
            }
        } else if mime == "application/json" || mime.starts_with("text/") {
            let s = str::from_utf8(data).unwrap_or("");
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
    let mut fsize = f64::from(u32::try_from(size).unwrap_or(u32::MAX));
    let mut i = 0;
    while fsize >= 1024.0 && i < units.len() - 1 {
        fsize /= 1024.0;
        i += 1;
    }
    format!("{:.0} {}", fsize, units[i])
}
