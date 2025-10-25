use std::{
  io::{self, Read, Write},
  os::fd::IntoRawFd,
  process::Command,
};

use clap::{ArgAction, Parser};
use wl_clipboard_rs::{
  copy::{
    ClipboardType as CopyClipboardType,
    MimeType as CopyMimeType,
    Options,
    Seat as CopySeat,
    ServeRequests,
    Source,
  },
  paste::{
    ClipboardType as PasteClipboardType,
    Error as PasteError,
    MimeType as PasteMimeType,
    Seat as PasteSeat,
    get_contents,
    get_mime_types,
  },
  utils::{PrimarySelectionCheckError, is_primary_selection_supported},
};

/// Extract the base name from argv[0].
fn get_base(argv0: &str) -> &str {
  std::path::Path::new(argv0)
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("")
}

/// Dispatch multicall binary logic based on argv[0].
/// Returns true if a multicall command was handled and the process should exit.
pub fn multicall_dispatch() -> bool {
  let argv0 = std::env::args().next().unwrap_or_default();
  let base = get_base(&argv0);
  match base {
    "stash-copy" | "wl-copy" => {
      wl_copy_main();
      true
    },
    "stash-paste" | "wl-paste" => {
      wl_paste_main();
      true
    },
    _ => false,
  }
}

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

fn wl_copy_main() {
  let args = WlCopyArgs::parse();

  if args.check_primary {
    match is_primary_selection_supported() {
      Ok(true) => {
        log::info!("primary selection is supported.");
        std::process::exit(0);
      },
      Ok(false) => {
        log::info!("primary selection is NOT supported.");
        std::process::exit(1);
      },
      Err(PrimarySelectionCheckError::NoSeats) => {
        log::error!("could not determine: no seats available.");
        std::process::exit(2);
      },
      Err(PrimarySelectionCheckError::MissingProtocol) => {
        log::error!("data-control protocol not supported by compositor.");
        std::process::exit(3);
      },
      Err(e) => {
        log::error!("error checking primary selection support: {e}");
        std::process::exit(4);
      },
    }
  }

  let clipboard = if args.primary {
    CopyClipboardType::Primary
  } else {
    CopyClipboardType::Regular
  };

  let mime_type = if let Some(mt) = args.mime_type.as_deref() {
    if mt == "text" || mt == "text/plain" {
      CopyMimeType::Text
    } else if mt == "autodetect" {
      CopyMimeType::Autodetect
    } else {
      CopyMimeType::Specific(mt.to_string())
    }
  } else {
    CopyMimeType::Autodetect
  };

  // Handle clear operation
  if args.clear {
    let mut opts = Options::new();
    opts.clipboard(clipboard);
    if let Some(seat_name) = args.seat.as_deref() {
      opts.seat(CopySeat::Specific(seat_name.to_string()));
    } else {
      opts.seat(CopySeat::All);
    }

    if let Err(e) = opts.copy(Source::Bytes(Vec::new().into()), mime_type) {
      log::error!("failed to clear clipboard: {e}");
      std::process::exit(1);
    }
    return;
  }

  // Read input data
  let input: Vec<u8> = if args.text.is_empty() {
    let mut buffer = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut buffer) {
      eprintln!("failed to read stdin: {e}");
      std::process::exit(1);
    }
    buffer
  } else {
    args.text.join(" ").into_bytes()
  };

  // Configure copy options
  let mut opts = Options::new();
  opts.clipboard(clipboard);

  if let Some(seat_name) = args.seat.as_deref() {
    opts.seat(CopySeat::Specific(seat_name.to_string()));
  } else {
    opts.seat(CopySeat::All);
  }

  if args.trim_newline {
    opts.trim_newline(true);
  }

  if args.omit_additional_text_mime_types {
    opts.omit_additional_text_mime_types(true);
  }

  // Configure serving behavior
  if args.paste_once {
    opts.serve_requests(ServeRequests::Only(1));
  } else if let Some(n) = args.serve_requests {
    opts.serve_requests(ServeRequests::Only(n));
  }

  // Handle foreground vs background mode
  if args.foreground {
    // Foreground mode: copy and serve in current process
    if let Err(e) = opts.copy(Source::Bytes(input.into()), mime_type) {
      log::error!("failed to copy to clipboard: {e}");
      std::process::exit(1);
    }
  } else {
    // Background mode: fork and let child serve requests
    // First prepare the copy to validate before forking
    let mut opts_fg = opts.clone();
    opts_fg.foreground(true);

    let prepared_copy =
      match opts_fg.prepare_copy(Source::Bytes(input.into()), mime_type) {
        Ok(copy) => copy,
        Err(e) => {
          log::error!("failed to prepare copy: {e}");
          std::process::exit(1);
        },
      };

    // Fork the process
    match unsafe { libc::fork() } {
      -1 => {
        log::error!("failed to fork: {}", std::io::Error::last_os_error());
        std::process::exit(1);
      },
      0 => {
        // Child process: serve clipboard requests
        // Redirect stdin/stdout to /dev/null to detach from terminal
        if let Ok(dev_null) = std::fs::OpenOptions::new()
          .read(true)
          .write(true)
          .open("/dev/null")
        {
          let fd = dev_null.into_raw_fd();
          unsafe {
            libc::dup2(fd, libc::STDIN_FILENO);
            libc::dup2(fd, libc::STDOUT_FILENO);
            libc::close(fd);
          }
        }

        // Serve clipboard requests
        if let Err(e) = prepared_copy.serve() {
          log::error!("failed to serve clipboard: {e}");
          std::process::exit(1);
        }
        std::process::exit(0);
      },
      _ => {
        // Parent process: exit immediately
        std::process::exit(0);
      },
    }
  }
}

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

fn wl_paste_main() {
  let args = WlPasteArgs::parse();

  let clipboard = if args.primary {
    PasteClipboardType::Primary
  } else {
    PasteClipboardType::Regular
  };

  let seat = if let Some(seat_name) = args.seat.as_deref() {
    PasteSeat::Specific(seat_name)
  } else {
    PasteSeat::Unspecified
  };

  // Handle list-types option
  if args.list_types {
    match get_mime_types(clipboard, seat) {
      Ok(types) => {
        for mime_type in types {
          println!("{}", mime_type);
        }
        std::process::exit(0);
      },
      Err(PasteError::NoSeats) => {
        log::error!("no seats available (is a Wayland compositor running?)");
        std::process::exit(1);
      },
      Err(e) => {
        log::error!("failed to list types: {e}");
        std::process::exit(1);
      },
    }
  }

  // Handle watch mode
  if let Some(watch_args) = args.watch {
    if watch_args.is_empty() {
      eprintln!("--watch requires a command to run");
      std::process::exit(1);
    }

    // For now, implement a simple version that just runs once
    // Full watch mode would require more complex implementation
    log::warn!("watch mode is not fully implemented in this version");

    let mut cmd = Command::new(&watch_args[0]);
    if watch_args.len() > 1 {
      cmd.args(&watch_args[1..]);
    }

    // Get clipboard content and pipe it to the command
    match get_contents(clipboard, seat, PasteMimeType::Text) {
      Ok((mut reader, _types)) => {
        let mut content = Vec::new();
        if let Err(e) = reader.read_to_end(&mut content) {
          log::error!("failed to read clipboard: {e}");
          std::process::exit(1);
        }

        // Set environment variable for clipboard state
        unsafe {
          std::env::set_var(
            "CLIPBOARD_STATE",
            if content.is_empty() { "nil" } else { "data" },
          )
        };

        // Spawn the command with the content as stdin
        use std::process::Stdio;
        cmd.stdin(Stdio::piped());

        let mut child = match cmd.spawn() {
          Ok(child) => child,
          Err(e) => {
            log::error!("failed to spawn command: {e}");
            std::process::exit(1);
          },
        };

        if let Some(stdin) = child.stdin.take() {
          use std::io::Write;
          let mut stdin = stdin;
          if let Err(e) = stdin.write_all(&content) {
            log::error!("failed to write to command stdin: {e}");
            std::process::exit(1);
          }
        }

        match child.wait() {
          Ok(status) => {
            std::process::exit(status.code().unwrap_or(1));
          },
          Err(e) => {
            log::error!("failed to wait for command: {e}");
            std::process::exit(1);
          },
        }
      },
      Err(PasteError::NoSeats) => {
        log::error!("no seats available (is a Wayland compositor running?)");
        std::process::exit(1);
      },
      Err(PasteError::ClipboardEmpty) => {
        unsafe {
          std::env::set_var("CLIPBOARD_STATE", "nil");
        }
        // Run command with /dev/null as stdin
        use std::process::Stdio;
        cmd.stdin(Stdio::null());

        match cmd.status() {
          Ok(status) => {
            std::process::exit(status.code().unwrap_or(1));
          },
          Err(e) => {
            log::error!("failed to run command: {e}");
            std::process::exit(1);
          },
        }
      },
      Err(e) => {
        log::error!("clipboard error: {e}");
        std::process::exit(1);
      },
    }
  }

  // Regular paste mode
  let mime_type = match args.mime_type.as_deref() {
    None | Some("text" | "autodetect") => PasteMimeType::Text,
    Some(other) => PasteMimeType::Specific(other),
  };

  match get_contents(clipboard, seat, mime_type) {
    Ok((mut reader, _types)) => {
      let mut out = io::stdout();
      let mut buf = Vec::new();
      match reader.read_to_end(&mut buf) {
        Ok(n) => {
          if n == 0 && args.no_newline {
            std::process::exit(1);
          }
          if let Err(e) = out.write_all(&buf) {
            log::error!("failed to write to stdout: {e}");
            std::process::exit(1);
          }
          if !args.no_newline && !buf.ends_with(b"\n") {
            if let Err(e) = out.write_all(b"\n") {
              log::error!("failed to write newline to stdout: {e}");
              std::process::exit(1);
            }
          }
        },
        Err(e) => {
          log::error!("failed to read clipboard: {e}");
          std::process::exit(1);
        },
      }
    },
    Err(PasteError::NoSeats) => {
      log::error!("no seats available (is a Wayland compositor running?)");
      std::process::exit(1);
    },
    Err(PasteError::ClipboardEmpty) => {
      if args.no_newline {
        std::process::exit(1);
      }
      // Otherwise, exit successfully with no output
    },
    Err(PasteError::NoMimeType) => {
      log::error!("clipboard does not contain requested MIME type");
      std::process::exit(1);
    },
    Err(e) => {
      log::error!("clipboard error: {e}");
      std::process::exit(1);
    },
  }
}
