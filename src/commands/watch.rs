use std::{
  collections::hash_map::DefaultHasher,
  hash::{Hash, Hasher},
  io::Read,
  time::Duration,
};

use smol::Timer;
use wl_clipboard_rs::paste::{ClipboardType, Seat, get_contents};

use crate::db::{ClipboardDb, SqliteClipboardDb};

pub trait WatchCommand {
  fn watch(
    &self,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: &[String],
  );
}

impl WatchCommand for SqliteClipboardDb {
  fn watch(
    &self,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: &[String],
  ) {
    smol::block_on(async {
      log::info!("Starting clipboard watch daemon");

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
                let id = self.next_sequence();
                match self.store_entry(
                  &buf[..],
                  max_dedupe_search,
                  max_items,
                  Some(excluded_apps),
                ) {
                  Ok(_) => {
                    log::info!("Stored new clipboard entry (id: {id})");
                    last_hash = Some(current_hash);
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
        Timer::after(Duration::from_millis(500)).await;
      }
    });
  }
}
