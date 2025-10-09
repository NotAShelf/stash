use std::io::{self, Read, Write};

use clap::{ArgAction, Parser};
use wl_clipboard_rs::paste::{
  ClipboardType,
  Error,
  MimeType,
  Seat,
  get_contents,
};

/// Dispatch multicall binary logic based on argv[0].
/// Returns true if a multicall command was handled and the process should exit.
pub fn multicall_dispatch() -> bool {
  let argv0 = std::env::args().next().unwrap_or_default();
  let base = std::path::Path::new(&argv0)
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("");
  match base {
    "stash-copy" | "wl-copy" => {
      multicall_stash_copy();
      true
    },
    "stash-paste" | "wl-paste" => {
      multicall_stash_paste();
      true
    },
    _ => false,
  }
}

#[allow(clippy::too_many_lines)]
fn multicall_stash_copy() {
  use clap::{ArgAction, Parser};
  use wl_clipboard_rs::{
    copy::{ClipboardType, MimeType, Options, ServeRequests, Source},
    utils::{PrimarySelectionCheckError, is_primary_selection_supported},
  };
  #[derive(Parser, Debug)]
  #[command(
    name = "stash-copy",
    about = "Copy clipboard contents on Wayland.",
    version,
    disable_help_subcommand = true
  )]
  #[allow(clippy::struct_excessive_bools)]
  struct Args {
    /// Serve only a single paste request and then exit
    #[arg(short = 'o', long = "paste-once", action = ArgAction::SetTrue)]
    paste_once:                      bool,
    /// Stay in the foreground instead of forking
    #[arg(short = 'f', long = "foreground", action = ArgAction::SetTrue)]
    foreground:                      bool,
    /// Clear the clipboard instead of copying
    #[arg(short = 'c', long = "clear", action = ArgAction::SetTrue)]
    clear:                           bool,
    /// Use the \"primary\" clipboard
    #[arg(short = 'p', long = "primary", action = ArgAction::SetTrue)]
    primary:                         bool,
    /// Use the regular clipboard
    #[arg(short = 'r', long = "regular", action = ArgAction::SetTrue)]
    regular:                         bool,
    /// Trim the trailing newline character before copying
    #[arg(short = 'n', long = "trim-newline", action = ArgAction::SetTrue)]
    trim_newline:                    bool,
    /// Pick the seat to work with
    #[arg(short = 's', long = "seat")]
    seat:                            Option<String>,
    /// Override the inferred MIME type for the content
    #[arg(short = 't', long = "type")]
    mime_type:                       Option<String>,
    /// Enable verbose logging
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    verbose:                         u8,
    /// Check if primary selection is supported and exit
    #[arg(long = "check-primary", action = ArgAction::SetTrue)]
    check_primary:                   bool,
    /// Do not offer additional text mime types (stash extension)
    #[arg(long = "omit-additional-text-mime-types", action = ArgAction::SetTrue, hide = true)]
    omit_additional_text_mime_types: bool,
    /// Number of paste requests to serve before exiting (stash extension)
    #[arg(short = 'x', long = "serve-requests", hide = true)]
    serve_requests:                  Option<usize>,
    /// Text to copy (if not given, read from stdin)
    #[arg(value_name = "TEXT TO COPY", action = ArgAction::Append)]
    text:                            Vec<String>,
  }

  let args = Args::parse();

  if args.check_primary {
    match is_primary_selection_supported() {
      Ok(true) => {
        log::info!("Primary selection is supported.");
        std::process::exit(0);
      },
      Ok(false) => {
        log::info!("Primary selection is NOT supported.");
        std::process::exit(1);
      },
      Err(PrimarySelectionCheckError::NoSeats) => {
        log::error!("Could not determine: no seats available.");
        std::process::exit(2);
      },
      Err(PrimarySelectionCheckError::MissingProtocol) => {
        log::error!("Data-control protocol not supported by compositor.");
        std::process::exit(3);
      },
      Err(e) => {
        log::error!("Error checking primary selection support: {e}");
        std::process::exit(4);
      },
    }
  }

  let clipboard = if args.primary {
    ClipboardType::Primary
  } else {
    ClipboardType::Regular
  };

  let mime_type = if let Some(mt) = args.mime_type.as_deref() {
    if mt == "text" || mt == "text/plain" {
      MimeType::Text
    } else if mt == "autodetect" {
      MimeType::Autodetect
    } else {
      MimeType::Specific(mt.to_string())
    }
  } else {
    MimeType::Autodetect
  };

  let mut input: Vec<u8> = Vec::new();
  if args.text.is_empty() {
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
      eprintln!("stash-copy: failed to read stdin: {e}");
      std::process::exit(1);
    }
  } else {
    input = args.text.join(" ").into_bytes();
  }

  let mut opts = Options::new();
  opts.clipboard(clipboard);

  if args.trim_newline {
    opts.trim_newline(true);
  }
  if args.foreground {
    opts.foreground(true);
  }
  if let Some(seat) = args.seat.as_deref() {
    log::debug!(
      "stash-copy: --seat is not supported by stash (using default seat: \
       {seat})"
    );
  }
  if args.omit_additional_text_mime_types {
    opts.omit_additional_text_mime_types(true);
  }
  // --paste-once overrides serve-requests
  if args.paste_once {
    opts.serve_requests(ServeRequests::Only(1));
  } else if let Some(n) = args.serve_requests {
    opts.serve_requests(ServeRequests::Only(n));
  }
  // --clear
  if args.clear {
    // Clear clipboard by setting empty contents
    if let Err(e) = opts.copy(Source::Bytes(Vec::new().into()), mime_type) {
      log::error!("stash-copy: failed to clear clipboard: {e}");
      std::process::exit(1);
    }
    return;
  }
  if let Err(e) = opts.copy(Source::Bytes(input.into()), mime_type) {
    log::error!("stash-copy: failed to copy to clipboard: {e}");
    std::process::exit(1);
  }
}

fn multicall_stash_paste() {
  #[derive(Parser, Debug)]
  #[command(
    name = "stash-paste",
    about = "Paste clipboard contents on Wayland.",
    version,
    disable_help_subcommand = true
  )]
  struct Args {
    /// List the offered MIME types instead of pasting
    #[arg(short = 'l', long = "list-types", action = ArgAction::SetTrue)]
    list_types: bool,
    /// Use the "primary" clipboard
    #[arg(short = 'p', long = "primary", action = ArgAction::SetTrue)]
    primary:    bool,
    /// Do not append a newline character
    #[arg(short = 'n', long = "no-newline", action = ArgAction::SetTrue)]
    no_newline: bool,
    /// Pick the seat to work with
    #[arg(short = 's', long = "seat")]
    seat:       Option<String>,
    /// Request the given MIME type instead of inferring the MIME type
    #[arg(short = 't', long = "type")]
    mime_type:  Option<String>,
    /// Enable verbose logging
    #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
    verbose:    u8,
  }

  let args = Args::parse();

  let clipboard = if args.primary {
    ClipboardType::Primary
  } else {
    ClipboardType::Regular
  };

  if let Some(seat) = args.seat.as_deref() {
    log::debug!(
      "stash-paste: --seat is not supported by stash (using default seat: \
       {seat})"
    );
  }

  if args.list_types {
    match get_contents(clipboard, Seat::Unspecified, MimeType::Text) {
      Ok((_reader, available_types)) => {
        print!("{available_types}");
        std::process::exit(0);
      },
      Err(e) => {
        log::error!("stash-paste: failed to list types: {e}");
        std::process::exit(1);
      },
    }
  }

  let mime_type = match args.mime_type.as_deref() {
    None | Some("text" | "autodetect") => MimeType::Text,
    Some(other) => MimeType::Specific(other),
  };

  match get_contents(clipboard, Seat::Unspecified, mime_type) {
    Ok((mut reader, _types)) => {
      let mut out = io::stdout();
      let mut buf = Vec::new();
      match reader.read_to_end(&mut buf) {
        Ok(n) => {
          if n == 0 && args.no_newline {
            std::process::exit(1);
          }
          let _ = out.write_all(&buf);
          if !args.no_newline && !buf.ends_with(b"\n") {
            let _ = out.write_all(b"\n");
          }
        },
        Err(e) => {
          log::error!("stash-paste: failed to read clipboard: {e}");
          std::process::exit(1);
        },
      }
    },
    Err(Error::NoSeats) => {
      log::error!(
        "stash-paste: no seats available (is a Wayland compositor running?)"
      );
      std::process::exit(1);
    },
    Err(Error::ClipboardEmpty) => {
      if args.no_newline {
        std::process::exit(1);
      }
    },
    Err(Error::NoMimeType) => {
      log::error!(
        "stash-paste: clipboard does not contain requested MIME type"
      );
      std::process::exit(1);
    },
    Err(e) => {
      log::error!("stash-paste: clipboard error: {e}");
      std::process::exit(1);
    },
  }
}
