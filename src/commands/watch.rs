use std::{
  collections::{BinaryHeap, hash_map::DefaultHasher},
  hash::{Hash, Hasher},
  io::Read,
  time::Duration,
};

use smol::Timer;
use wl_clipboard_rs::paste::{ClipboardType, Seat, get_contents};

use crate::db::{ClipboardDb, SqliteClipboardDb};

/// Wrapper to provide Ord implementation for f64 by negating values.
/// This allows BinaryHeap (which is a max-heap) to function as a min-heap.
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
/// Uses Neg wrapper to turn BinaryHeap (max-heap) into min-heap behavior.
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
}

pub trait WatchCommand {
  fn watch(
    &self,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: &[String],
    expire_after: Option<Duration>,
  );
}

impl WatchCommand for SqliteClipboardDb {
  fn watch(
    &self,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: &[String],
    expire_after: Option<Duration>,
  ) {
    smol::block_on(async {
      log::info!("Starting clipboard watch daemon");

      // Cleanup any already-expired entries on startup
      if let Ok(count) = self.cleanup_expired() {
        if count > 0 {
          log::info!("Cleaned up {} expired entries on startup", count);
        }
      }

      // Build expiration queue from existing entries
      let mut exp_queue = ExpirationQueue::new();
      if let Ok(Some((expires_at, id))) = self.get_next_expiration() {
        exp_queue.push(expires_at, id);
        // Load remaining expirations
        let mut stmt = self
          .conn
          .prepare(
            "SELECT expires_at, id FROM clipboard WHERE expires_at IS NOT \
             NULL ORDER BY expires_at ASC",
          )
          .ok();
        if let Some(ref mut stmt) = stmt {
          let mut rows = stmt.query([]).ok();
          if let Some(ref mut rows) = rows {
            while let Ok(Some(row)) = rows.next() {
              if let (Ok(exp), Ok(row_id)) =
                (row.get::<_, f64>(0), row.get::<_, i64>(1))
              {
                // Skip first entry which is already added
                if exp_queue
                  .heap
                  .iter()
                  .any(|(_, existing_id)| *existing_id == row_id)
                {
                  continue;
                }
                exp_queue.push(exp, row_id);
              }
            }
          }
        }
      }

      // We use hashes for comparison instead of storing full contents
      let mut last_hash: Option<u64> = None;
      let mut buf = Vec::with_capacity(4096);

      // Helper to hash clipboard contents
      let hash_contents = |data: &[u8]| -> u64 {
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        hasher.finish()
      };

      // Initialize with current clipboard
      if let Ok((mut reader, _)) = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        wl_clipboard_rs::paste::MimeType::Any,
      ) {
        buf.clear();
        if reader.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
          last_hash = Some(hash_contents(&buf));
        }
      }

      loop {
        // Process any pending expirations
        if let Some(next_exp) = exp_queue.peek_next() {
          let now = SqliteClipboardDb::now();
          if next_exp <= now {
            // Expired entries to process
            let expired_ids = exp_queue.pop_expired(now);
            for id in expired_ids {
              // Verify entry still exists (handles stale heap entries)
              let exists = self
                .conn
                .query_row(
                  "SELECT 1 FROM clipboard WHERE id = ?1",
                  [id],
                  |_| Ok(()),
                )
                .is_ok();
              if exists {
                self
                  .conn
                  .execute("DELETE FROM clipboard WHERE id = ?1", [id])
                  .ok();
                log::info!("Entry {id} expired and removed");
              }
            }
          } else {
            // Sleep precisely until next expiration (sub-second precision)
            let sleep_duration = next_exp - now;
            Timer::after(Duration::from_secs_f64(sleep_duration)).await;
            continue; // Skip normal poll, process expirations first
          }
        }

        // Normal clipboard polling
        match get_contents(
          ClipboardType::Regular,
          Seat::Unspecified,
          wl_clipboard_rs::paste::MimeType::Any,
        ) {
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
                match self.store_entry(
                  &buf[..],
                  max_dedupe_search,
                  max_items,
                  Some(excluded_apps),
                ) {
                  Ok(id) => {
                    log::info!("Stored new clipboard entry (id: {id})");
                    last_hash = Some(current_hash);

                    // Set expiration if configured
                    if let Some(duration) = expire_after {
                      let expires_at =
                        SqliteClipboardDb::now() + duration.as_secs_f64();
                      self.set_expiration(id, expires_at).ok();
                      exp_queue.push(expires_at, id);
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

        // Normal poll interval (only if no expirations pending)
        if exp_queue.peek_next().is_none() {
          Timer::after(Duration::from_millis(500)).await;
        }
      }
    });
  }
}
