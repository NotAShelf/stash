use std::{io::Read, time::Duration};

use smol::Timer;
use wl_clipboard_rs::paste::{ClipboardType, Seat, get_contents};

use crate::db::{ClipboardDb, Entry, SqliteClipboardDb};

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

      // Preallocate buffer for clipboard contents
      let mut last_contents: Option<Vec<u8>> = None;
      let mut buf = Vec::with_capacity(4096); // reasonable default, hopefully

      // Initialize with current clipboard to avoid duplicating on startup
      if let Ok((mut reader, _)) = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        wl_clipboard_rs::paste::MimeType::Any,
      ) {
        buf.clear();
        if reader.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
          last_contents = Some(buf.clone());
        }
      }

      loop {
        match get_contents(
          ClipboardType::Regular,
          Seat::Unspecified,
          wl_clipboard_rs::paste::MimeType::Any,
        ) {
          Ok((mut reader, mime_type)) => {
            buf.clear();
            if let Err(e) = reader.read_to_end(&mut buf) {
              log::error!("Failed to read clipboard contents: {e}");
              Timer::after(Duration::from_millis(500)).await;
              continue;
            }

            // Only store if changed and not empty
            if !buf.is_empty() && (last_contents.as_ref() != Some(&buf)) {
              let new_contents = std::mem::take(&mut buf);
              let mime = Some(mime_type.to_string());
              let entry = Entry {
                contents: new_contents.clone(),
                mime,
              };
              let id = self.next_sequence();
              match self.store_entry(
                &entry.contents[..],
                max_dedupe_search,
                max_items,
                Some(excluded_apps),
              ) {
                Ok(_) => {
                  log::info!("Stored new clipboard entry (id: {id})");
                  last_contents = Some(new_contents);
                },
                Err(crate::db::StashError::ExcludedByApp(_)) => {
                  log::info!("Clipboard entry excluded by app filter");
                  last_contents = Some(new_contents);
                },
                Err(crate::db::StashError::Store(ref msg))
                  if msg.contains("Excluded by app filter") =>
                {
                  log::info!("Clipboard entry excluded by app filter");
                  last_contents = Some(new_contents);
                },
                Err(e) => {
                  log::error!("Failed to store clipboard entry: {e}");
                  last_contents = Some(new_contents);
                },
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
