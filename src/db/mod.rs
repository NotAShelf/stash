use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::str;

use image::{GenericImageView, ImageFormat};
use log::{error, info, warn};
use rmp_serde::{decode::from_read, encode::to_vec};
use serde::{Deserialize, Serialize};
use sled::{Db, IVec};

pub trait ClipboardDb {
    fn store_entry(&self, input: impl Read, max_dedupe_search: u64, max_items: u64);
    fn deduplicate(&self, buf: &[u8], max: u64);
    fn trim_db(&self, max: u64);
    fn delete_last(&self);
    fn wipe_db(&self);
    fn list_entries(&self, out: impl Write, preview_width: u32);
    fn decode_entry(&self, in_: impl Read, out: impl Write, input: Option<String>);
    fn delete_query(&self, query: &str);
    fn delete_entries(&self, in_: impl Read);
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
    fn store_entry(&self, mut input: impl Read, max_dedupe_search: u64, max_items: u64) {
        let mut buf = Vec::new();
        if input.read_to_end(&mut buf).is_err() || buf.is_empty() || buf.len() > 5 * 1_000_000 {
            warn!("Input is empty or too large, skipping store.");
            return;
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            warn!("Input is all whitespace, skipping store.");
            return;
        }

        let mime = detect_mime(&buf);

        self.deduplicate(&buf, max_dedupe_search);

        let entry = Entry {
            contents: buf.clone(),
            mime,
        };

        let id = self.next_sequence();
        let enc = match to_vec(&entry) {
            Ok(enc) => enc,
            Err(e) => {
                error!("Failed to serialize entry: {e}");
                return;
            }
        };

        match self.db.insert(u64_to_ivec(id), enc) {
            Ok(_) => info!("Stored entry with id {id}"),
            Err(e) => error!("Failed to store entry: {e}"),
        }
        self.trim_db(max_items);
    }

    fn deduplicate(&self, buf: &[u8], max: u64) {
        let mut count = 0;
        let mut deduped = 0;
        for item in self.db.iter().rev().take(max as usize) {
            let (k, v) = match item {
                Ok((k, v)) => (k, v),
                Err(e) => {
                    error!("Error reading entry during deduplication: {e}");
                    continue;
                }
            };
            let entry: Entry = match from_read(v.as_ref()) {
                Ok(e) => e,
                Err(e) => {
                    error!("Error decoding entry during deduplication: {e}");
                    continue;
                }
            };
            if entry.contents == buf {
                match self.db.remove(k) {
                    Ok(_) => {
                        deduped += 1;
                        info!("Deduplicated an entry");
                    }
                    Err(e) => error!("Failed to remove entry during deduplication: {e}"),
                }
            }
            count += 1;
            if count >= max {
                break;
            }
        }
        if deduped > 0 {
            info!("Deduplicated {deduped} entries");
        }
    }

    fn trim_db(&self, max: u64) {
        let mut keys: Vec<_> = self
            .db
            .iter()
            .rev()
            .filter_map(|kv| match kv {
                Ok((k, _)) => Some(k),
                Err(e) => {
                    error!("Failed to read key during trim: {e}");
                    None
                }
            })
            .collect();
        let initial_len = keys.len();
        if keys.len() as u64 > max {
            for k in keys.drain((max as usize)..) {
                match self.db.remove(k) {
                    Ok(_) => info!("Trimmed entry from database"),
                    Err(e) => error!("Failed to trim entry: {e}"),
                }
            }
            info!(
                "Trimmed {} entries from database",
                initial_len - max as usize
            );
        }
    }

    fn delete_last(&self) {
        if let Some((k, _)) = self.db.iter().next_back().and_then(Result::ok) {
            match self.db.remove(k) {
                Ok(_) => info!("Deleted last entry"),
                Err(e) => error!("Failed to delete last entry: {e}"),
            }
        } else {
            warn!("No entries to delete");
        }
    }

    fn wipe_db(&self) {
        match self.db.clear() {
            Ok(()) => info!("Wiped database"),
            Err(e) => error!("Failed to wipe database: {e}"),
        }
    }

    fn list_entries(&self, mut out: impl Write, preview_width: u32) {
        let mut listed = 0;
        for (k, v) in self.db.iter().rev().filter_map(Result::ok) {
            let id = ivec_to_u64(&k);
            let entry: Entry = match from_read(v.as_ref()) {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to decode entry during list: {e}");
                    continue;
                }
            };
            let preview = preview_entry(&entry.contents, entry.mime.as_deref(), preview_width);
            if writeln!(out, "{id}\t{preview}").is_ok() {
                listed += 1;
            }
        }
        info!("Listed {listed} entries");
    }

    fn decode_entry(&self, mut in_: impl Read, mut out: impl Write, input: Option<String>) {
        let s = if let Some(input) = input {
            input
        } else {
            let mut buf = String::new();
            if let Err(e) = in_.read_to_string(&mut buf) {
                error!("Failed to read input for decode: {e}");
                return;
            }
            buf
        };
        let id = match extract_id(&s) {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to extract id for decode: {e}");
                return;
            }
        };
        let v = match self.db.get(u64_to_ivec(id)) {
            Ok(Some(v)) => v,
            Ok(None) => {
                warn!("No entry found for id {id}");
                return;
            }
            Err(e) => {
                error!("Failed to get entry for decode: {e}");
                return;
            }
        };
        let entry: Entry = match from_read(v.as_ref()) {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to decode entry: {e}");
                return;
            }
        };
        if let Err(e) = out.write_all(&entry.contents) {
            error!("Failed to write decoded entry: {e}");
        } else {
            info!("Decoded entry with id {id}");
        }
    }

    fn delete_query(&self, query: &str) {
        let mut deleted = 0;
        for (k, v) in self.db.iter().filter_map(Result::ok) {
            let entry: Entry = match from_read(v.as_ref()) {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to decode entry during query delete: {e}");
                    continue;
                }
            };
            if entry
                .contents
                .windows(query.len())
                .any(|w| w == query.as_bytes())
            {
                match self.db.remove(k) {
                    Ok(_) => {
                        deleted += 1;
                        info!("Deleted entry matching query");
                    }
                    Err(e) => error!("Failed to delete entry during query delete: {e}"),
                }
            }
        }
        info!("Deleted {deleted} entries matching query '{query}'");
    }

    fn delete_entries(&self, in_: impl Read) {
        let reader = BufReader::new(in_);
        let mut deleted = 0;
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(id) = extract_id(&line) {
                match self.db.remove(u64_to_ivec(id)) {
                    Ok(_) => {
                        deleted += 1;
                        info!("Deleted entry with id {id}");
                    }
                    Err(e) => error!("Failed to delete entry with id {id}: {e}"),
                }
            } else {
                warn!("Failed to extract id from line: {line}");
            }
        }
        info!("Deleted {deleted} entries by id from stdin");
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
    let mut fsize = size as f64;
    let mut i = 0;
    while fsize >= 1024.0 && i < units.len() - 1 {
        fsize /= 1024.0;
        i += 1;
    }
    format!("{:.0} {}", fsize, units[i])
}
