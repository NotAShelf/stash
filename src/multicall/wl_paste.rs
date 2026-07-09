// https://wayland.freedesktop.org/docs/html/apa.html#protocol-spec-wl_data_device
// https://docs.rs/wl-clipboard-rs/latest/wl_clipboard_rs
// https://github.com/YaLTeR/wl-clipboard-rs/blob/master/wl-clipboard-rs-tools/src/bin/wl_paste.rs
use std::{
  collections::hash_map::DefaultHasher,
  hash::{Hash, Hasher},
  io::{self, Read, Write},
  process::{Command, Stdio},
  sync::{Arc, Mutex},
  thread,
  time::{Duration, Instant},
};

use clap::{ArgAction, Parser};
use color_eyre::eyre::{Context, Result, bail};
use log::LevelFilter;
use wl_clipboard_rs::paste::{
  ClipboardType as PasteClipboardType,
  Error as PasteError,
  MimeType as PasteMimeType,
  Seat as PasteSeat,
  get_contents,
  get_mime_types,
};

// Watch mode timing constants
const WATCH_POLL_INTERVAL_MS: u64 = 500;
const WATCH_DEBOUNCE_INTERVAL_MS: u64 = 1000;

// Maximum clipboard content size to prevent memory exhaustion (100MB)
const MAX_CLIPBOARD_SIZE: usize = 100 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
  name = "wl-paste",
  about = "Paste clipboard contents on Wayland.",
  version,
  disable_help_subcommand = true
)]
struct WlPasteArgs {
  /// List the offered MIME types instead of pasting
  #[arg(short = 'l', long = "list-types", action = ArgAction::SetTrue)]
  list_types: bool,

  /// Use the "primary" clipboard
  #[arg(short = 'p', long = "primary", action = ArgAction::SetTrue)]
  primary: bool,

  /// Do not append a newline character
  #[arg(short = 'n', long = "no-newline", action = ArgAction::SetTrue)]
  no_newline: bool,

  /// Pick the seat to work with
  #[arg(short = 's', long = "seat")]
  seat: Option<String>,

  /// Request the given MIME type instead of inferring the MIME type
  #[arg(short = 't', long = "type")]
  mime_type: Option<String>,

  /// Enable verbose logging
  #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
  verbose: u8,

  /// Watch for clipboard changes and run a command
  #[arg(short = 'w', long = "watch")]
  watch: Option<Vec<String>>,
}

fn get_paste_mime_type(mime_arg: Option<&str>) -> PasteMimeType<'_> {
  match mime_arg {
    None | Some("text" | "autodetect") => PasteMimeType::Text,
    Some(other) => PasteMimeType::Specific(other),
  }
}

fn init_logger(verbose: u8) {
  let level = match verbose {
    0 => LevelFilter::Warn,
    1 => LevelFilter::Info,
    2 => LevelFilter::Debug,
    _ => LevelFilter::Trace,
  };
  let _ = env_logger::Builder::new().filter_level(level).try_init();
}

fn handle_list_types(
  clipboard: PasteClipboardType,
  seat: PasteSeat,
) -> Result<()> {
  match get_mime_types(clipboard, seat) {
    Ok(types) => {
      for mime_type in types {
        println!("{mime_type}");
      }

      Ok(())
    },
    Err(PasteError::NoSeats) => {
      bail!("no seats available (is a Wayland compositor running?)");
    },
    Err(e) => {
      bail!("failed to list types: {e}");
    },
  }
}

fn handle_watch_mode(
  args: &WlPasteArgs,
  clipboard: PasteClipboardType,
  seat: PasteSeat,
) -> Result<()> {
  let watch_args = args.watch.as_ref().unwrap();
  if watch_args.is_empty() {
    bail!("--watch requires a command to run");
  }

  log::info!("starting clipboard watch mode");

  // Shared state for tracking last content and shutdown signal
  let last_content_hash = Arc::new(Mutex::new(None::<u64>));
  let shutdown = Arc::new(Mutex::new(false));

  // Set up signal handler for graceful shutdown
  let shutdown_clone = shutdown.clone();
  ctrlc::set_handler(move || {
    log::info!("received shutdown signal, stopping watch mode");
    if let Ok(mut shutdown_guard) = shutdown_clone.lock() {
      *shutdown_guard = true;
    } else {
      log::error!("failed to acquire shutdown lock in signal handler");
    }
  })
  .context("failed to set signal handler")?;

  let poll_interval = Duration::from_millis(WATCH_POLL_INTERVAL_MS);
  let debounce_interval = Duration::from_millis(WATCH_DEBOUNCE_INTERVAL_MS);
  let mut last_change_time = Instant::now();

  loop {
    // Check for shutdown signal
    match shutdown.lock() {
      Ok(shutdown_guard) => {
        if *shutdown_guard {
          log::info!("shutting down watch mode");
          break Ok(());
        }
      },
      Err(e) => {
        log::error!("failed to acquire shutdown lock: {e}");
        thread::sleep(poll_interval);
        continue;
      },
    }

    // Get current clipboard content
    let current_hash = match get_clipboard_content_hash(
      clipboard,
      seat,
      args.mime_type.as_deref(),
    ) {
      Ok(hash) => hash,
      Err(e) => {
        log::error!("failed to get clipboard content hash: {e}");
        thread::sleep(poll_interval);
        continue;
      },
    };

    // Check if content has changed
    match last_content_hash.lock() {
      Ok(mut last_hash_guard) => {
        let changed = *last_hash_guard != Some(current_hash);
        if changed {
          let now = Instant::now();

          // Debounce rapid changes
          if now.duration_since(last_change_time) >= debounce_interval {
            *last_hash_guard = Some(current_hash);
            last_change_time = now;
            drop(last_hash_guard); // Release lock before spawning command

            log::info!("clipboard content changed, executing watch command");

            // Execute the watch command
            if let Err(e) = execute_watch_command(
              watch_args,
              clipboard,
              seat,
              args.mime_type.as_deref(),
            ) {
              log::error!("failed to execute watch command: {e}");
              // Continue watching even if command fails
            }
          }
        }
        changed
      },
      Err(e) => {
        log::error!("failed to acquire last_content_hash lock: {e}");
        thread::sleep(poll_interval);
        continue;
      },
    };

    thread::sleep(poll_interval);
  }
}

fn get_clipboard_content_hash(
  clipboard: PasteClipboardType,
  seat: PasteSeat,
  mime_arg: Option<&str>,
) -> Result<u64> {
  match get_contents(clipboard, seat, get_paste_mime_type(mime_arg)) {
    Ok((mut reader, _types)) => {
      let mut content = Vec::new();
      let mut temp_buffer = [0; 8192];

      loop {
        let bytes_read = reader
          .read(&mut temp_buffer)
          .context("failed to read clipboard content")?;

        if bytes_read == 0 {
          break;
        }

        if content.len() + bytes_read > MAX_CLIPBOARD_SIZE {
          bail!(
            "clipboard content exceeds maximum size of {} bytes",
            MAX_CLIPBOARD_SIZE
          );
        }

        content.extend_from_slice(&temp_buffer[..bytes_read]);
      }

      let mut hasher = DefaultHasher::new();
      content.hash(&mut hasher);
      Ok(hasher.finish())
    },
    Err(PasteError::ClipboardEmpty) => {
      Ok(0) // Empty clipboard has hash 0
    },
    Err(e) => bail!("clipboard error: {e}"),
  }
}

/// Validate command name.
fn validate_command_name(cmd: &str) -> Result<()> {
  if cmd.is_empty() {
    bail!("command name cannot be empty");
  }
  Ok(())
}

/// Set environment variable safely with validation
fn set_clipboard_state_env(has_content: bool) -> Result<()> {
  let value = if has_content { "data" } else { "nil" };

  // Validate the environment variable value
  if !matches!(value, "data" | "nil") {
    bail!("invalid clipboard state value: {value}");
  }

  // SAFETY: watch mode is single-threaded here and mutates the environment
  // immediately before spawning the child command.
  unsafe {
    std::env::set_var("STASH_CLIPBOARD_STATE", value);
  }
  Ok(())
}

fn execute_watch_command(
  watch_args: &[String],
  clipboard: PasteClipboardType,
  seat: PasteSeat,
  mime_arg: Option<&str>,
) -> Result<()> {
  if watch_args.is_empty() {
    bail!("watch command cannot be empty");
  }

  // Validate command name for security
  validate_command_name(&watch_args[0])?;

  let mut cmd = Command::new(&watch_args[0]);
  if watch_args.len() > 1 {
    cmd.args(&watch_args[1..]);
  }

  // Get clipboard content and pipe it to the command
  match get_contents(clipboard, seat, get_paste_mime_type(mime_arg)) {
    Ok((mut reader, _types)) => {
      let mut content = Vec::new();
      let mut temp_buffer = [0; 8192];

      loop {
        let bytes_read = reader
          .read(&mut temp_buffer)
          .context("failed to read clipboard")?;

        if bytes_read == 0 {
          break;
        }

        if content.len() + bytes_read > MAX_CLIPBOARD_SIZE {
          bail!(
            "clipboard content exceeds maximum size of {} bytes",
            MAX_CLIPBOARD_SIZE
          );
        }

        content.extend_from_slice(&temp_buffer[..bytes_read]);
      }

      // Set environment variable safely
      set_clipboard_state_env(!content.is_empty())?;

      // Spawn the command with the content as stdin
      cmd.stdin(Stdio::piped());

      let mut child = cmd.spawn()?;

      if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        if let Err(e) = stdin.write_all(&content) {
          bail!("failed to write to command stdin: {e}");
        }
      }

      match child.wait() {
        Ok(status) => {
          if !status.success() {
            log::warn!("watch command exited with status: {status}");
          }
        },
        Err(e) => {
          bail!("failed to wait for command: {e}");
        },
      }
    },
    Err(PasteError::ClipboardEmpty) => {
      // Set environment variable safely
      set_clipboard_state_env(false)?;

      // Run command with /dev/null as stdin
      cmd.stdin(Stdio::null());

      match cmd.status() {
        Ok(status) => {
          if !status.success() {
            log::warn!("watch command exited with status: {status}");
          }
        },
        Err(e) => {
          bail!("failed to run command: {e}");
        },
      }
    },
    Err(e) => {
      bail!("clipboard error: {e}");
    },
  }

  Ok(())
}

/// Whether a MIME type is an HTML-flavored representation that should be
/// deprioritized. Covers `text/html` (with optional parameters) and Firefox's
/// Mozilla-internal wrappers (`text/_moz_htmlcontext`, ...), which are UTF-16
/// encoded and truncate at the first NUL byte when treated as text.
fn is_html_like(mime: &str) -> bool {
  mime == "text/html"
    || mime.starts_with("text/html;")
    || mime.starts_with("text/_moz")
}

/// Select the best MIME type from available types when none is specified.
///
/// The offered types arrive as an unordered [`HashSet`], so selection is made
/// deterministic by resolving ties with lexicographic order. Preference,
/// highest first:
///
/// 1. A concrete non-text type (`image/*`, `application/pdf`, ...).
/// 2. Plain text: `text/plain;charset=*`, then `text/plain`, then any other
///    non-HTML `text/*`.
/// 3. Generic X11 text selections: `UTF8_STRING`, `STRING`, `TEXT`.
/// 4. HTML-flavored wrappers, then anything else, only as a last resort.
///
/// HTML-flavored types are deprioritized throughout: browsers list them first
/// even when a cleaner representation exists, and they are frequently UTF-16
/// encoded.
fn select_best_mime_type(
  types: &std::collections::HashSet<String>,
) -> Option<String> {
  if types.is_empty() {
    return None;
  }

  // Deterministic view of the offered types (HashSet has no stable order).
  let mut sorted: Vec<&String> = types.iter().collect();
  sorted.sort();

  let first_match = |pred: &dyn Fn(&str) -> bool| -> Option<String> {
    sorted
      .iter()
      .find(|m| pred(m.as_str()))
      .map(|m| (*m).clone())
  };

  // 1. Concrete non-text type (image/*, application/*, ...), never HTML-like.
  if let Some(m) = first_match(&|m| m.contains('/') && !m.starts_with("text/"))
  {
    return Some(m);
  }

  // 2. Plain text, most specific first.
  if let Some(m) = first_match(&|m| m.starts_with("text/plain;charset=")) {
    return Some(m);
  }
  if let Some(m) = first_match(&|m| m == "text/plain") {
    return Some(m);
  }
  if let Some(m) = first_match(&|m| m.starts_with("text/") && !is_html_like(m))
  {
    return Some(m);
  }

  // 3. Generic X11 text selections, in a fixed preference order.
  for fallback in ["UTF8_STRING", "STRING", "TEXT"] {
    if types.contains(fallback) {
      return Some(fallback.to_string());
    }
  }

  // 4. HTML wrapper or anything else, deterministically.
  first_match(&|m| !is_html_like(m))
    .or_else(|| sorted.first().map(|m| (*m).clone()))
}

fn handle_regular_paste(
  args: &WlPasteArgs,
  clipboard: PasteClipboardType,
  seat: PasteSeat,
) -> Result<()> {
  // If no MIME type specified, select the best available MIME type
  let available_types = if args.mime_type.is_none() {
    get_mime_types(clipboard, seat).ok()
  } else {
    None
  };

  let selected_type = available_types.as_ref().and_then(select_best_mime_type);

  let mime_type = if let Some(ref best) = selected_type {
    log::debug!("auto-selecting MIME type: {best}");
    PasteMimeType::Specific(best)
  } else {
    get_paste_mime_type(args.mime_type.as_deref())
  };

  match get_contents(clipboard, seat, mime_type) {
    Ok((mut reader, types)) => {
      let mut out = io::stdout();
      let mut buf = Vec::new();
      let mut temp_buffer = [0; 8192];

      loop {
        let bytes_read = reader
          .read(&mut temp_buffer)
          .context("failed to read clipboard")?;

        if bytes_read == 0 {
          break;
        }

        if buf.len() + bytes_read > MAX_CLIPBOARD_SIZE {
          bail!(
            "clipboard content exceeds maximum size of {} bytes",
            MAX_CLIPBOARD_SIZE
          );
        }

        buf.extend_from_slice(&temp_buffer[..bytes_read]);
      }

      if buf.is_empty() && args.no_newline {
        bail!("no content available and --no-newline specified");
      }
      if let Err(e) = out.write_all(&buf) {
        if e.kind() == io::ErrorKind::BrokenPipe {
          return Ok(());
        }
        bail!("failed to write to stdout: {e}");
      }

      // Only add newline for text content, not binary data
      // Check if the MIME type indicates text content
      let is_text_content = if types.is_empty() {
        // If no MIME type, check if content is valid UTF-8
        std::str::from_utf8(&buf).is_ok()
      } else {
        types.starts_with("text/")
          || types == "application/json"
          || types == "application/xml"
          || types == "application/x-sh"
      };

      if !args.no_newline
        && is_text_content
        && !buf.ends_with(b"\n")
        && let Err(e) = out.write_all(b"\n")
        && e.kind() != io::ErrorKind::BrokenPipe
      {
        bail!("failed to write newline to stdout: {e}");
      }
    },
    Err(PasteError::NoSeats) => {
      bail!("no seats available (is a Wayland compositor running?)");
    },
    Err(PasteError::ClipboardEmpty) => {
      if args.no_newline {
        bail!("clipboard empty and --no-newline specified");
      }
      // Otherwise, exit successfully with no output
    },
    Err(PasteError::NoMimeType) => {
      bail!("clipboard does not contain requested MIME type");
    },
    Err(e) => {
      bail!("clipboard error: {e}");
    },
  }

  Ok(())
}

pub fn wl_paste_main() -> Result<()> {
  let args = WlPasteArgs::parse();
  init_logger(args.verbose);

  let clipboard = if args.primary {
    PasteClipboardType::Primary
  } else {
    PasteClipboardType::Regular
  };
  let seat = args
    .seat
    .as_deref()
    .map_or(PasteSeat::Unspecified, PasteSeat::Specific);

  // Handle list-types option
  if args.list_types {
    handle_list_types(clipboard, seat)?;
    return Ok(());
  }

  // Handle watch mode
  if args.watch.is_some() {
    handle_watch_mode(&args, clipboard, seat)?;
    return Ok(());
  }

  // Regular paste mode
  handle_regular_paste(&args, clipboard, seat)?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

  use super::*;

  fn set(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
  }

  #[test]
  fn test_empty_returns_none() {
    assert_eq!(select_best_mime_type(&set(&[])), None);
  }

  #[test]
  fn test_single_type() {
    assert_eq!(
      select_best_mime_type(&set(&["text/plain"])).as_deref(),
      Some("text/plain")
    );
  }

  #[test]
  fn test_deterministic_across_runs() {
    // A HashSet's iteration order is unspecified; the same offer set must
    // always resolve to the same MIME type regardless of internal ordering.
    let offered = set(&[
      "text/html",
      "text/_moz_htmlcontext",
      "text/plain",
      "UTF8_STRING",
    ]);
    let first = select_best_mime_type(&offered);
    for _ in 0..100 {
      assert_eq!(select_best_mime_type(&offered), first);
    }
    // ...and it must be the plain-text representation, not an HTML wrapper.
    assert_eq!(first.as_deref(), Some("text/plain"));
  }

  #[test]
  fn test_prefers_concrete_non_text() {
    let offered = set(&["text/html", "image/png", "text/plain"]);
    assert_eq!(
      select_best_mime_type(&offered).as_deref(),
      Some("image/png")
    );
  }

  #[test]
  fn test_prefers_charset_plain_over_bare_plain() {
    let offered = set(&["text/plain", "text/plain;charset=utf-8"]);
    assert_eq!(
      select_best_mime_type(&offered).as_deref(),
      Some("text/plain;charset=utf-8")
    );
  }

  #[test]
  fn test_plain_wins_over_html() {
    let offered = set(&["text/html", "text/plain"]);
    assert_eq!(
      select_best_mime_type(&offered).as_deref(),
      Some("text/plain")
    );
  }

  #[test]
  fn test_generic_selection_fallback() {
    let offered = set(&["UTF8_STRING", "STRING", "TEXT"]);
    assert_eq!(
      select_best_mime_type(&offered).as_deref(),
      Some("UTF8_STRING")
    );
  }

  #[test]
  fn test_html_only_as_last_resort() {
    let offered = set(&["text/html"]);
    assert_eq!(
      select_best_mime_type(&offered).as_deref(),
      Some("text/html")
    );
  }

  #[test]
  fn test_multiple_images_deterministic() {
    // Ties between concrete types resolve lexicographically.
    let offered = set(&["image/png", "image/jpeg"]);
    assert_eq!(
      select_best_mime_type(&offered).as_deref(),
      Some("image/jpeg")
    );
  }
}
