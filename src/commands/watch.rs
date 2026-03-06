use std::{collections::BinaryHeap, io::Read, time::Duration};

/// FNV-1a hasher for deterministic hashing across process runs.
/// Unlike `DefaultHasher` (`SipHash`), this produces stable hashes.
struct Fnv1aHasher {
  state: u64,
}

impl Fnv1aHasher {
  const FNV_OFFSET: u64 = 0xCBF29CE484222325;
  const FNV_PRIME: u64 = 0x100000001B3;

  fn new() -> Self {
    Self {
      state: Self::FNV_OFFSET,
    }
  }

  fn write(&mut self, bytes: &[u8]) {
    for byte in bytes {
      self.state ^= u64::from(*byte);
      self.state = self.state.wrapping_mul(Self::FNV_PRIME);
    }
  }

  fn finish(&self) -> u64 {
    self.state
  }
}

use smol::Timer;
use wl_clipboard_rs::{
  copy::{MimeType as CopyMimeType, Options, Source},
  paste::{
    ClipboardType,
    MimeType as PasteMimeType,
    Seat,
    get_contents,
    get_mime_types_ordered,
  },
};

use crate::db::{SqliteClipboardDb, nonblocking::AsyncClipboardDb};

/// Wrapper to provide [`Ord`] implementation for `f64` by negating values.
/// This allows [`BinaryHeap`], which is a max-heap, to function as a min-heap.
/// Also see:
///  - <https://doc.rust-lang.org/std/cmp/struct.Reverse.html>
///  - <https://doc.rust-lang.org/std/primitive.f64.html#method.total_cmp>
///  - <https://docs.rs/ordered-float/latest/ordered_float/>
#[derive(Debug, Clone, Copy)]
struct Neg(f64);

impl Neg {
  fn inner(&self) -> f64 {
    self.0
  }
}

impl std::cmp::PartialEq for Neg {
  fn eq(&self, other: &Self) -> bool {
    self.0 == other.0
  }
}

impl std::cmp::Eq for Neg {}

impl std::cmp::PartialOrd for Neg {
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    Some(self.cmp(other))
  }
}

impl std::cmp::Ord for Neg {
  fn cmp(&self, other: &Self) -> std::cmp::Ordering {
    // Reverse ordering for min-heap behavior
    other
      .0
      .partial_cmp(&self.0)
      .unwrap_or(std::cmp::Ordering::Equal)
  }
}

/// Min-heap for tracking entry expirations with sub-second precision.
/// Uses Neg wrapper to turn `BinaryHeap` (max-heap) into min-heap behavior.
#[derive(Debug, Default)]
struct ExpirationQueue {
  heap: BinaryHeap<(Neg, i64)>,
}

impl ExpirationQueue {
  /// Create a new empty expiration queue
  fn new() -> Self {
    Self {
      heap: BinaryHeap::new(),
    }
  }

  /// Push a new expiration into the queue
  fn push(&mut self, expires_at: f64, id: i64) {
    self.heap.push((Neg(expires_at), id));
  }

  /// Peek at the next expiration timestamp without removing it
  fn peek_next(&self) -> Option<f64> {
    self.heap.peek().map(|(neg, _)| neg.inner())
  }

  /// Remove and return all entries that have expired by `now`
  fn pop_expired(&mut self, now: f64) -> Vec<i64> {
    let mut expired = Vec::new();
    while let Some((neg_exp, id)) = self.heap.peek() {
      let expires_at = neg_exp.inner();
      if expires_at <= now {
        expired.push(*id);
        self.heap.pop();
      } else {
        break;
      }
    }
    expired
  }

  /// Check if the queue is empty
  fn is_empty(&self) -> bool {
    self.heap.is_empty()
  }

  /// Get the number of entries in the queue
  fn len(&self) -> usize {
    self.heap.len()
  }
}

/// Get clipboard contents using the source application's preferred MIME type.
///
/// See, `MimeType::Any` lets wl-clipboard-rs pick a type in arbitrary order,
/// which causes issues when applications offer multiple types (e.g. file
/// managers offering `text/uri-list` + `text/plain`, or Firefox offering
/// `text/html` + `image/png` + `text/plain`).
///
/// This queries the ordered types via [`get_mime_types_ordered`], which
/// preserves the Wayland protocol's offer order (source application's
/// preference) and then requests the first type with [`MimeType::Specific`].
///
/// The two-step approach has a theoretical race (clipboard could change between
/// the calls), but the wl-clipboard-rs API has no single-call variant that
/// respects source ordering. A race simply produces an error that the polling
/// loop handles like any other clipboard-empty/error case.
///
/// When `preference` is `"text"`, uses `MimeType::Text` directly (single call).
/// When `preference` is `"image"`, picks the first offered `image/*` type.
/// Otherwise picks the source's first offered type.
fn negotiate_mime_type(
  preference: &str,
) -> Result<(Box<dyn Read>, String), wl_clipboard_rs::paste::Error> {
  if preference == "text" {
    let (reader, mime_str) = get_contents(
      ClipboardType::Regular,
      Seat::Unspecified,
      PasteMimeType::Text,
    )?;
    return Ok((Box::new(reader) as Box<dyn Read>, mime_str));
  }

  let offered =
    get_mime_types_ordered(ClipboardType::Regular, Seat::Unspecified)?;

  let chosen = if preference == "image" {
    // Pick the first offered image type, fall back to first overall
    offered
      .iter()
      .find(|m| m.starts_with("image/"))
      .or_else(|| offered.first())
  } else {
    // XXX: When preference is "any", deprioritize text/html if a more
    // concrete type is available. Browsers and Electron apps put
    // text/html first even for "Copy Image", but the HTML is just
    // a wrapper (<img src="...">), i.e., never what the user wants in a
    // clipboard manager. Prefer image/* first, then any non-html
    // type, and fall back to text/html only as a last resort.
    let has_image = offered.iter().any(|m| m.starts_with("image/"));
    if has_image {
      offered
        .iter()
        .find(|m| m.starts_with("image/"))
        .or_else(|| offered.first())
    } else if offered.first().is_some_and(|m| m == "text/html") {
      offered
        .iter()
        .find(|m| *m != "text/html")
        .or_else(|| offered.first())
    } else {
      offered.first()
    }
  };

  match chosen {
    Some(mime_str) => {
      let (reader, actual_mime) = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        PasteMimeType::Specific(mime_str),
      )?;
      Ok((Box::new(reader) as Box<dyn Read>, actual_mime))
    },
    None => Err(wl_clipboard_rs::paste::Error::NoSeats),
  }
}

#[allow(clippy::too_many_arguments)]
pub trait WatchCommand {
  async fn watch(
    &self,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: &[String],
    expire_after: Option<Duration>,
    mime_type_preference: &str,
    min_size: Option<usize>,
    max_size: usize,
  );
}

impl WatchCommand for SqliteClipboardDb {
  async fn watch(
    &self,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: &[String],
    expire_after: Option<Duration>,
    mime_type_preference: &str,
    min_size: Option<usize>,
    max_size: usize,
  ) {
    let async_db = AsyncClipboardDb::new(self.db_path.clone());
    log::info!(
      "Starting clipboard watch daemon with MIME type preference: \
       {mime_type_preference}"
    );

    // Build expiration queue from existing entries
    let mut exp_queue = ExpirationQueue::new();

    // Load all expirations from database asynchronously
    match async_db.load_all_expirations().await {
      Ok(expirations) => {
        for (expires_at, id) in expirations {
          exp_queue.push(expires_at, id);
        }
        if !exp_queue.is_empty() {
          log::info!("Loaded {} expirations from database", exp_queue.len());
        }
      },
      Err(e) => {
        log::warn!("Failed to load expirations: {e}");
      },
    }

    // We use hashes for comparison instead of storing full contents
    let mut last_hash: Option<u64> = None;
    let mut buf = Vec::with_capacity(4096);

    // Helper to hash clipboard contents using FNV-1a (deterministic across
    // runs)
    let hash_contents = |data: &[u8]| -> u64 {
      let mut hasher = Fnv1aHasher::new();
      hasher.write(data);
      hasher.finish()
    };

    // Initialize with current clipboard using smart MIME negotiation
    if let Ok((mut reader, _)) = negotiate_mime_type(mime_type_preference) {
      buf.clear();
      if reader.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
        last_hash = Some(hash_contents(&buf));
      }
    }

    let poll_interval = Duration::from_millis(500);

    loop {
      // Process any pending expirations that are due now
      if let Some(next_exp) = exp_queue.peek_next() {
        let now = SqliteClipboardDb::now();
        if next_exp <= now {
          // Expired entries to process
          let expired_ids = exp_queue.pop_expired(now);
          for id in expired_ids {
            // Verify entry still exists and get its content_hash
            let expired_hash: Option<i64> =
              match async_db.get_content_hash(id).await {
                Ok(hash) => hash,
                Err(e) => {
                  log::warn!("Failed to get content hash for entry {id}: {e}");
                  None
                },
              };

            if let Some(stored_hash) = expired_hash {
              // Mark as expired
              if let Err(e) = async_db.mark_expired(id).await {
                log::warn!("Failed to mark entry {id} as expired: {e}");
              } else {
                log::info!("Entry {id} marked as expired");
              }

              // Check if this expired entry is currently in the clipboard
              if let Ok((mut reader, _)) =
                negotiate_mime_type(mime_type_preference)
              {
                let mut current_buf = Vec::new();
                if reader.read_to_end(&mut current_buf).is_ok()
                  && !current_buf.is_empty()
                {
                  let current_hash = hash_contents(&current_buf);
                  // Convert stored i64 to u64 for comparison (preserves bit
                  // pattern)
                  if current_hash == stored_hash as u64 {
                    // Clear the clipboard since expired content is still
                    // there
                    let mut opts = Options::new();
                    opts
                      .clipboard(wl_clipboard_rs::copy::ClipboardType::Regular);
                    if opts
                      .copy(
                        Source::Bytes(Vec::new().into()),
                        CopyMimeType::Autodetect,
                      )
                      .is_ok()
                    {
                      log::info!(
                        "Cleared clipboard containing expired entry {id}"
                      );
                      last_hash = None; // reset tracked hash
                    } else {
                      log::warn!(
                        "Failed to clear clipboard for expired entry {id}"
                      );
                    }
                  }
                }
              }
            }
          }
        }
      }

      // Normal clipboard polling (always run, even when expirations are
      // pending)
      match negotiate_mime_type(mime_type_preference) {
        Ok((mut reader, _mime_type)) => {
          buf.clear();
          if let Err(e) = reader.read_to_end(&mut buf) {
            log::error!("Failed to read clipboard contents: {e}");
            Timer::after(Duration::from_millis(500)).await;
            continue;
          }

          // Only store if changed and not empty
          if !buf.is_empty() {
            let current_hash = hash_contents(&buf);
            if last_hash != Some(current_hash) {
              // Clone buf for the async operation since it needs 'static
              let buf_clone = buf.clone();
              #[allow(clippy::cast_possible_wrap)]
              let content_hash = Some(current_hash as i64);
              match async_db
                .store_entry(
                  buf_clone,
                  max_dedupe_search,
                  max_items,
                  Some(excluded_apps.to_vec()),
                  min_size,
                  max_size,
                  content_hash,
                )
                .await
              {
                Ok(id) => {
                  log::info!("Stored new clipboard entry (id: {id})");
                  last_hash = Some(current_hash);

                  // Set expiration if configured
                  if let Some(duration) = expire_after {
                    let expires_at =
                      SqliteClipboardDb::now() + duration.as_secs_f64();
                    if let Err(e) =
                      async_db.set_expiration(id, expires_at).await
                    {
                      log::warn!(
                        "Failed to set expiration for entry {id}: {e}"
                      );
                    } else {
                      exp_queue.push(expires_at, id);
                    }
                  }
                },
                Err(crate::db::StashError::ExcludedByApp(_)) => {
                  log::info!("Clipboard entry excluded by app filter");
                  last_hash = Some(current_hash);
                },
                Err(crate::db::StashError::Store(ref msg))
                  if msg.contains("Excluded by app filter") =>
                {
                  log::info!("Clipboard entry excluded by app filter");
                  last_hash = Some(current_hash);
                },
                Err(e) => {
                  log::error!("Failed to store clipboard entry: {e}");
                  last_hash = Some(current_hash);
                },
              }
            }
          }
        },
        Err(e) => {
          let error_msg = e.to_string();
          if !error_msg.contains("empty") {
            log::error!("Failed to get clipboard contents: {e}");
          }
        },
      }

      // Calculate sleep time: min of poll interval and time until next
      // expiration
      let sleep_duration = if let Some(next_exp) = exp_queue.peek_next() {
        let now = SqliteClipboardDb::now();
        let time_to_exp = (next_exp - now).max(0.0);
        poll_interval.min(Duration::from_secs_f64(time_to_exp))
      } else {
        poll_interval
      };
      Timer::after(sleep_duration).await;
    }
  }
}

/// Given ordered offers and a preference, return the
/// chosen MIME type. This mirrors the selection logic in
/// [`negotiate_mime_type`] without requiring a Wayland connection.
#[cfg(test)]
fn pick_mime<'a>(
  offered: &'a [String],
  preference: &str,
) -> Option<&'a String> {
  if preference == "image" {
    offered
      .iter()
      .find(|m| m.starts_with("image/"))
      .or_else(|| offered.first())
  } else {
    let has_image = offered.iter().any(|m| m.starts_with("image/"));
    if has_image {
      offered
        .iter()
        .find(|m| m.starts_with("image/"))
        .or_else(|| offered.first())
    } else if offered.first().is_some_and(|m| m == "text/html") {
      offered
        .iter()
        .find(|m| *m != "text/html")
        .or_else(|| offered.first())
    } else {
      offered.first()
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_pick_first_offered() {
    let offered = vec!["text/uri-list".to_string(), "text/plain".to_string()];
    assert_eq!(pick_mime(&offered, "any").unwrap(), "text/uri-list");
  }

  #[test]
  fn test_pick_image_preference_finds_image() {
    let offered = vec![
      "text/html".to_string(),
      "image/png".to_string(),
      "text/plain".to_string(),
    ];
    assert_eq!(pick_mime(&offered, "image").unwrap(), "image/png");
  }

  #[test]
  fn test_pick_image_preference_falls_back() {
    let offered = vec!["text/html".to_string(), "text/plain".to_string()];
    // No image types offered — falls back to first
    assert_eq!(pick_mime(&offered, "image").unwrap(), "text/html");
  }

  #[test]
  fn test_pick_empty_offered() {
    let offered: Vec<String> = vec![];
    assert!(pick_mime(&offered, "any").is_none());
  }

  #[test]
  fn test_pick_image_over_html_firefox_copy_image() {
    // Firefox "Copy Image" offers html first, then image, then text.
    // We should pick the image, not the html wrapper.
    let offered = vec![
      "text/html".to_string(),
      "image/png".to_string(),
      "text/plain".to_string(),
    ];
    assert_eq!(pick_mime(&offered, "any").unwrap(), "image/png");
  }

  #[test]
  fn test_pick_image_over_html_electron() {
    // Electron apps also put text/html before image types
    let offered = vec!["text/html".to_string(), "image/jpeg".to_string()];
    assert_eq!(pick_mime(&offered, "any").unwrap(), "image/jpeg");
  }

  #[test]
  fn test_pick_html_fallback_when_only_html() {
    // When text/html is the only type, pick it
    let offered = vec!["text/html".to_string()];
    assert_eq!(pick_mime(&offered, "any").unwrap(), "text/html");
  }

  #[test]
  fn test_pick_text_over_html_when_no_image() {
    // Rich text copy: html + plain, no image — prefer plain text
    let offered = vec!["text/html".to_string(), "text/plain".to_string()];
    assert_eq!(pick_mime(&offered, "any").unwrap(), "text/plain");
  }

  #[test]
  fn test_pick_file_manager_uri_list_first() {
    // File managers typically offer uri-list first
    let offered = vec!["text/uri-list".to_string(), "text/plain".to_string()];
    assert_eq!(pick_mime(&offered, "any").unwrap(), "text/uri-list");
  }
}
