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

fn handle_list_types(
  clipboard: PasteClipboardType,
  seat: PasteSeat,
) -> Result<()> {
  match get_mime_types(clipboard, seat) {
    Ok(types) => {
      for mime_type in types {
        println!("{mime_type}");
      }

      #[allow(clippy::needless_return)]
      return Ok(());
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
    let current_hash = match get_clipboard_content_hash(clipboard, seat) {
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
            if let Err(e) = execute_watch_command(watch_args, clipboard, seat) {
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
) -> Result<u64> {
  match get_contents(clipboard, seat, PasteMimeType::Text) {
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

/// Validate command name to prevent command injection
fn validate_command_name(cmd: &str) -> Result<()> {
  if cmd.is_empty() {
    bail!("command name cannot be empty");
  }

  // Reject commands with shell metacharacters or path traversal
  if cmd.contains(|c| {
    ['|', '&', ';', '$', '`', '(', ')', '<', '>', '"', '\'', '\\'].contains(&c)
  }) {
    bail!("command contains invalid characters: {cmd}");
  }

  // Reject absolute paths and relative path traversal
  if cmd.starts_with('/') || cmd.contains("..") {
    bail!("command paths are not allowed: {cmd}");
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

  // Safe to set environment variable with validated, known-safe value
  unsafe {
    std::env::set_var("STASH_CLIPBOARD_STATE", value);
  }
  Ok(())
}

fn execute_watch_command(
  watch_args: &[String],
  clipboard: PasteClipboardType,
  seat: PasteSeat,
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
  match get_contents(clipboard, seat, PasteMimeType::Text) {
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

fn handle_regular_paste(
  args: &WlPasteArgs,
  clipboard: PasteClipboardType,
  seat: PasteSeat,
) -> Result<()> {
  let mime_type = get_paste_mime_type(args.mime_type.as_deref());

  match get_contents(clipboard, seat, mime_type) {
    Ok((mut reader, _types)) => {
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
        bail!("failed to write to stdout: {e}");
      }
      if !args.no_newline && !buf.ends_with(b"\n") {
        if let Err(e) = out.write_all(b"\n") {
          bail!("failed to write newline to stdout: {e}");
        }
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
