use std::{
  collections::hash_map::DefaultHasher,
  hash::{Hash, Hasher},
  io::Read,
  time::Duration,
};

use smol::Timer;
use wl_clipboard_rs::paste::{
  ClipboardType,
  MimeType,
  Seat,
  get_contents as wl_get_contents,
  get_mime_types,
};

use crate::db::{ClipboardDb, SqliteClipboardDb};

/// Get clipboard contents with optional smart MIME type selection.
///
/// Provides intelligent clipboard content retrieval that can
/// prioritize specific MIME types based on user preferences or built-in
/// heuristics.
///
/// # Arguments
///
/// * `clipboard` - The clipboard type to retrieve from (`Regular`, `Primary`,
///   etc.)
/// * `seat` - The Wayland seat identifier
/// * `preferred_types` - List of MIME types to prioritize in order. Supports
///   wildcards like `"image/*"` or `"text/*"`. Empty list enables default smart
///   detection.
/// * `smart_detection` - When true, enables intelligent MIME type selection.
///   When false, falls back to [`MimeType::Any`] behavior.
///
/// # Returns
///
/// Returns a tuple containing:
/// - A [`Box<dyn Read>`] for reading the clipboard content
/// - A [`String`] representing the actual MIME type that was used
///
/// # Errors
///
/// Returns errors if:
///
/// - Clipboard access fails
/// - MIME type negotiation fails
/// - Content reading fails
fn get_contents(
  clipboard: ClipboardType,
  seat: Seat,
  types_preferred: &[String],
  detection_smart: bool,
) -> Result<(Box<dyn std::io::Read>, String), Box<dyn std::error::Error>> {
  log::debug!(
    "attempted to get clipboard contents with \
     smart_detection={detection_smart}, preferred_types={types_preferred:?}"
  );

  if !types_preferred.is_empty() && detection_smart {
    log::debug!("querying available mime types with user preferences");
    if let Ok(types) = get_mime_types(clipboard, seat) {
      log::debug!("Available MIME types: {types:?}");
      log::debug!("trying user preferred types in order: {types_preferred:?}");

      for preferred in types_preferred {
        // Handle wildcards (e.g., "image/*")
        if preferred.ends_with("/*") {
          let prefix = &preferred[..preferred.len() - 2];
          for mime_type in &types {
            if mime_type.starts_with(prefix) {
              let mime_str = mime_type.clone();
              let (reader, _) = wl_get_contents(
                clipboard,
                seat,
                MimeType::Specific(&mime_str),
              )?;
              return Ok((
                Box::new(reader) as Box<dyn std::io::Read>,
                mime_str,
              ));
            }
          }
          log::warn!("no matches found for wildcard pattern '{preferred}'");
        } else {
          // Exact match
          if types.contains(preferred) {
            log::debug!("selected MIME type '{preferred}' (exact match)");
            let (reader, _) =
              wl_get_contents(clipboard, seat, MimeType::Specific(preferred))?;
            return Ok((
              Box::new(reader) as Box<dyn std::io::Read>,
              preferred.clone(),
            ));
          }
          log::info!("exact match '{preferred}' not found in available types");
        }
      }
      log::warn!(
        "none of the preferred types matched available types, falling back to \
         default priority"
      );
    } else {
      // Fall back to regular behavior if mime type query fails
      log::warn!("failed to query available MIME types, falling back to Any");
    }
  } else if detection_smart {
    // Default for "smart" detection:
    // prioritize images > text/plain > other text > other
    // It is as smart as I am, and to be honest, that's not very smart
    if let Ok(types) = get_mime_types(clipboard, seat) {
      log::debug!("available MIME types: {types:?}");

      // Priority order: images > text/plain > other text > other
      for mime_type in &types {
        if mime_type.starts_with("image/") {
          let mime_str = mime_type.clone();
          let (reader, _) =
            wl_get_contents(clipboard, seat, MimeType::Specific(&mime_str))?;
          return Ok((Box::new(reader) as Box<dyn std::io::Read>, mime_str));
        }
      }

      log::debug!("no image formats found, checking for text/plain");
      if types.contains("text/plain") {
        let (reader, _) = wl_get_contents(clipboard, seat, MimeType::Text)?;
        return Ok((
          Box::new(reader) as Box<dyn std::io::Read>,
          "text/plain".to_string(),
        ));
      }

      log::debug!("no text/plain found, checking for other text formats");
      for mime_type in &types {
        if mime_type.starts_with("text/") {
          let mime_str = mime_type.clone();
          let (reader, _) =
            wl_get_contents(clipboard, seat, MimeType::Specific(&mime_str))?;
          return Ok((Box::new(reader) as Box<dyn std::io::Read>, mime_str));
        }
      }

      // Fallback to first available
      log::info!("no preferred formats found, using first available type");
      if let Some(first_type) = types.iter().next() {
        let mime_str = first_type.clone();
        let (reader, _) =
          wl_get_contents(clipboard, seat, MimeType::Specific(&mime_str))?;
        return Ok((Box::new(reader) as Box<dyn std::io::Read>, mime_str));
      }

      log::warn!("no MIME types available from clipboard");
    } else {
      // Fall back to regular behavior if mime type query fails
      log::warn!("failed to query available MIME types, falling back to Any");
    }
  } else {
    log::debug!("smart MIME detection is not enabled, using MimeType::Any");
  }

  // Fallback to Any if smart detection is disabled or fails
  let (reader, _) = wl_get_contents(clipboard, seat, MimeType::Any)?;
  log::info!("selected MIME type 'application/octet-stream'");
  Ok((
    Box::new(reader) as Box<dyn std::io::Read>,
    "application/octet-stream".to_string(),
  ))
}

pub trait WatchCommand {
  fn watch(
    &self,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: &[String],
    preferred_types: &[String],
  );
}

impl WatchCommand for SqliteClipboardDb {
  fn watch(
    &self,
    max_dedupe_search: u64,
    max_items: u64,
    excluded_apps: &[String],
    preferred_types: &[String],
  ) {
    smol::block_on(async {
      log::info!("starting clipboard watch daemon");

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
        preferred_types,
        true, // enable smart detection
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
          preferred_types,
          true, // enable smart detection
        ) {
          Ok((mut reader, _mime_type)) => {
            buf.clear();
            if let Err(e) = reader.read_to_end(&mut buf) {
              log::error!("failed to read clipboard contents: {e}");
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
                    log::info!("stored new clipboard entry (id: {id})");
                    last_hash = Some(current_hash);
                  },
                  Err(crate::db::StashError::ExcludedByApp(_)) => {
                    log::info!("clipboard entry excluded by app filter");
                    last_hash = Some(current_hash);
                  },
                  Err(crate::db::StashError::Store(ref msg))
                    if msg.contains("excluded by app filter") =>
                  {
                    log::info!("clipboard entry excluded by app filter");
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
              log::error!("failed to get clipboard contents: {e}");
            }
          },
        }
        Timer::after(Duration::from_millis(500)).await;
      }
    });
  }
}
