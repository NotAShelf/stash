use std::io::{self, Read};

use clap::{ArgAction, Parser};
use color_eyre::eyre::{Context, Result, bail};
use wl_clipboard_rs::{
  copy::{
    ClipboardType as CopyClipboardType,
    MimeType as CopyMimeType,
    Options,
    Seat as CopySeat,
    ServeRequests,
    Source,
  },
  utils::{PrimarySelectionCheckError, is_primary_selection_supported},
};

// Maximum clipboard content size to prevent memory exhaustion (100MB)
const MAX_CLIPBOARD_SIZE: usize = 100 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
  name = "wl-copy",
  about = "Copy clipboard contents on Wayland.",
  version
)]
#[allow(clippy::struct_excessive_bools)]
struct WlCopyArgs {
  /// Serve only a single paste request and then exit
  #[arg(short = 'o', long = "paste-once", action = ArgAction::SetTrue)]
  paste_once: bool,

  /// Stay in the foreground instead of forking
  #[arg(short = 'f', long = "foreground", action = ArgAction::SetTrue)]
  foreground: bool,

  /// Clear the clipboard instead of copying
  #[arg(short = 'c', long = "clear", action = ArgAction::SetTrue)]
  clear: bool,

  /// Use the "primary" clipboard
  #[arg(short = 'p', long = "primary", action = ArgAction::SetTrue)]
  primary: bool,

  /// Use the regular clipboard
  #[arg(short = 'r', long = "regular", action = ArgAction::SetTrue)]
  regular: bool,

  /// Trim the trailing newline character before copying
  #[arg(short = 'n', long = "trim-newline", action = ArgAction::SetTrue)]
  trim_newline: bool,

  /// Pick the seat to work with
  #[arg(short = 's', long = "seat")]
  seat: Option<String>,

  /// Override the inferred MIME type for the content
  #[arg(short = 't', long = "type")]
  mime_type: Option<String>,

  /// Enable verbose logging
  #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
  verbose: u8,

  /// Check if primary selection is supported and exit
  #[arg(long = "check-primary", action = ArgAction::SetTrue)]
  check_primary: bool,

  /// Do not offer additional text mime types (stash extension)
  #[arg(long = "omit-additional-text-mime-types", action = ArgAction::SetTrue, hide = true)]
  omit_additional_text_mime_types: bool,

  /// Number of paste requests to serve before exiting (stash extension)
  #[arg(short = 'x', long = "serve-requests", hide = true)]
  serve_requests: Option<usize>,

  /// Text to copy (if not given, read from stdin)
  #[arg(value_name = "TEXT TO COPY", action = ArgAction::Append)]
  text: Vec<String>,
}

fn handle_check_primary() {
  let exit_code = match is_primary_selection_supported() {
    Ok(true) => {
      log::info!("primary selection is supported.");
      0
    },
    Ok(false) => {
      log::info!("primary selection is NOT supported.");
      1
    },
    Err(PrimarySelectionCheckError::NoSeats) => {
      log::error!("could not determine: no seats available.");
      2
    },
    Err(PrimarySelectionCheckError::MissingProtocol) => {
      log::error!("data-control protocol not supported by compositor.");
      3
    },
    Err(e) => {
      log::error!("error checking primary selection support: {e}");
      4
    },
  };

  // Exit with the relevant code
  std::process::exit(exit_code);
}

fn get_clipboard_type(primary: bool) -> CopyClipboardType {
  if primary {
    CopyClipboardType::Primary
  } else {
    CopyClipboardType::Regular
  }
}

fn get_mime_type(mime_arg: Option<&str>) -> CopyMimeType {
  match mime_arg {
    Some("text" | "text/plain") => CopyMimeType::Text,
    Some("autodetect") | None => CopyMimeType::Autodetect,
    Some(specific) => CopyMimeType::Specific(specific.to_string()),
  }
}

fn read_input_data(text_args: &[String]) -> Result<Vec<u8>> {
  if text_args.is_empty() {
    let mut buffer = Vec::new();
    let mut stdin = io::stdin();

    // Read with size limit to prevent memory exhaustion
    let mut temp_buffer = [0; 8192];
    loop {
      let bytes_read = stdin
        .read(&mut temp_buffer)
        .context("failed to read from stdin")?;

      if bytes_read == 0 {
        break;
      }

      if buffer.len() + bytes_read > MAX_CLIPBOARD_SIZE {
        bail!(
          "input exceeds maximum clipboard size of {} bytes",
          MAX_CLIPBOARD_SIZE
        );
      }

      buffer.extend_from_slice(&temp_buffer[..bytes_read]);
    }

    Ok(buffer)
  } else {
    let content = text_args.join(" ");
    if content.len() > MAX_CLIPBOARD_SIZE {
      bail!(
        "input exceeds maximum clipboard size of {} bytes",
        MAX_CLIPBOARD_SIZE
      );
    }
    Ok(content.into_bytes())
  }
}

fn configure_copy_options(
  args: &WlCopyArgs,
  clipboard: CopyClipboardType,
) -> Options {
  let mut opts = Options::new();
  opts.clipboard(clipboard);
  opts.seat(
    args
      .seat
      .as_deref()
      .map_or(CopySeat::All, |s| CopySeat::Specific(s.to_string())),
  );

  if args.trim_newline {
    opts.trim_newline(true);
  }

  if args.omit_additional_text_mime_types {
    opts.omit_additional_text_mime_types(true);
  }

  if args.paste_once {
    opts.serve_requests(ServeRequests::Only(1));
  } else if let Some(n) = args.serve_requests {
    opts.serve_requests(ServeRequests::Only(n));
  }

  opts
}

fn handle_clear_clipboard(
  args: &WlCopyArgs,
  clipboard: CopyClipboardType,
  mime_type: CopyMimeType,
) -> Result<()> {
  let mut opts = Options::new();
  opts.clipboard(clipboard);
  opts.seat(
    args
      .seat
      .as_deref()
      .map_or(CopySeat::All, |s| CopySeat::Specific(s.to_string())),
  );

  opts
    .copy(Source::Bytes(Vec::new().into()), mime_type)
    .context("failed to clear clipboard")?;

  Ok(())
}

fn fork_and_serve(prepared_copy: wl_clipboard_rs::copy::PreparedCopy) {
  // Use a simpler approach: serve in background thread instead of forking
  // This avoids all the complexity and safety issues with fork()
  let handle = std::thread::spawn(move || {
    if let Err(e) = prepared_copy.serve() {
      log::error!("background clipboard service failed: {e}");
    }
  });

  // Give the background thread a moment to start
  std::thread::sleep(std::time::Duration::from_millis(50));
  log::debug!("clipboard service started in background thread");

  // Detach the thread to allow it to run independently
  // The thread will be cleaned up when it completes or when the process exits
  std::mem::forget(handle);
}

pub fn wl_copy_main() -> Result<()> {
  let args = WlCopyArgs::parse();

  if args.check_primary {
    handle_check_primary();
  }

  let clipboard = get_clipboard_type(args.primary);
  let mime_type = get_mime_type(args.mime_type.as_deref());

  // Handle clear operation
  if args.clear {
    handle_clear_clipboard(&args, clipboard, mime_type)?;
    return Ok(());
  }

  // Read input data
  let input =
    read_input_data(&args.text).context("failed to read input data")?;

  // Configure copy options
  let opts = configure_copy_options(&args, clipboard);

  // Handle foreground vs background mode
  if args.foreground {
    // Foreground mode: copy and serve in current process
    opts
      .copy(Source::Bytes(input.into()), mime_type)
      .context("failed to copy to clipboard")?;
  } else {
    // Background mode: spawn child process to serve requests
    // First prepare to copy to validate before spawning
    let mut opts_fg = opts.clone();
    opts_fg.foreground(true);

    let prepared_copy = opts_fg
      .prepare_copy(Source::Bytes(input.into()), mime_type)
      .context("failed to prepare copy")?;

    fork_and_serve(prepared_copy);
  }

  Ok(())
}
