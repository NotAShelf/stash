mod clipboard;
mod commands;
mod db;
mod hash;
mod mime;
mod multicall;

use std::{
  env,
  io::{self, IsTerminal},
  path::PathBuf,
  time::Duration,
};

use clap::{CommandFactory, Parser, Subcommand};
use color_eyre::eyre::{self, bail};
use humantime::parse_duration;
use inquire::Confirm;

// The Wayland module is only compiled for focused-window detection, which
// depends on low-level compositor protocols.
#[cfg(feature = "use-toplevel")] mod wayland;

use crate::{
  commands::{
    decode::DecodeCommand,
    delete::DeleteCommand,
    import::ImportCommand,
    list::ListCommand,
    query::QueryCommand,
    store::StoreCommand,
    watch::WatchCommand,
  },
  db::{ClipboardDb, DEFAULT_MAX_ENTRY_SIZE},
};

#[derive(Parser)]
#[command(name = "stash")]
#[command(about = "Wayland clipboard manager", version)]
struct Cli {
  #[command(subcommand)]
  command: Option<Command>,

  /// Maximum number of clipboard entries to keep
  #[arg(long, default_value_t = u64::MAX)]
  max_items: u64,

  /// Number of recent entries to check for duplicates when storing new
  /// clipboard data.
  #[arg(long, default_value_t = 20)]
  max_dedupe_search: u64,

  /// Minimum size (in bytes) for clipboard entries. Entries smaller than this
  /// will not be stored.
  #[arg(long, env = "STASH_MIN_SIZE")]
  min_size: Option<usize>,

  /// Maximum size (in bytes) for clipboard entries. Entries larger than this
  /// will not be stored. Defaults to 5MB.
  #[arg(long, default_value_t = DEFAULT_MAX_ENTRY_SIZE, env = "STASH_MAX_SIZE")]
  max_size: usize,

  /// Maximum width (in characters) for clipboard entry previews in list
  /// output.
  #[arg(long, default_value_t = 100)]
  preview_width: u32,

  /// Path to the `SQLite` clipboard database file.
  #[arg(long, env = "STASH_DB_PATH")]
  db_path: Option<PathBuf>,

  /// Application names to exclude from clipboard history
  #[cfg(feature = "use-toplevel")]
  #[arg(long, value_delimiter = ',', env = "STASH_EXCLUDED_APPS")]
  excluded_apps: Vec<String>,

  /// Ask for confirmation before destructive operations
  #[arg(long)]
  ask: bool,

  #[command(flatten)]
  verbosity: clap_verbosity_flag::Verbosity,
}

#[derive(Subcommand)]
enum Command {
  /// Store clipboard contents
  Store,

  /// List clipboard history
  List {
    /// Output format: "tsv" (default) or "json"
    #[arg(long, value_parser = ["tsv", "json"])]
    format: Option<String>,

    /// Show only expired entries (diagnostic, does not remove them)
    #[arg(long)]
    expired: bool,

    /// Reverse the order of entries (oldest first instead of newest first)
    #[arg(long)]
    reverse: bool,
  },

  /// Decode and output clipboard entry by id
  Decode { input: Option<String> },

  /// Delete clipboard entry by id (if numeric), or entries matching a query (if
  /// not). Numeric arguments are treated as ids. Use --type to specify
  /// explicitly.
  Delete {
    /// Id or query string
    arg: Option<String>,

    /// Explicitly specify type: "id" or "query"
    #[arg(long, value_parser = ["id", "query"])]
    r#type: Option<String>,

    /// Ask for confirmation before deleting
    #[arg(long)]
    ask: bool,
  },

  /// Database management operations
  Db {
    #[command(subcommand)]
    action: DbAction,
  },

  /// Import clipboard data from stdin (default: TSV format)
  Import {
    /// Explicitly specify format: "tsv" (default)
    #[arg(long, value_parser = ["tsv"])]
    r#type: Option<String>,

    /// Ask for confirmation before importing
    #[arg(long)]
    ask: bool,
  },

  /// Start a process to watch clipboard for changes and store automatically.
  Watch {
    /// Expire new entries after duration (e.g., "3s", "500ms", "1h30m").
    #[arg(long, value_parser = parse_duration)]
    expire_after: Option<Duration>,

    /// MIME type preference for clipboard reading.
    #[arg(short = 't', long, default_value = "any")]
    mime_type: String,

    /// Persist clipboard contents after the source application closes.
    #[arg(long)]
    persist: bool,
  },
}

#[derive(Subcommand)]
enum DbAction {
  /// Wipe database entries
  Wipe {
    /// Only wipe expired entries instead of all entries
    #[arg(long)]
    expired: bool,

    /// Ask for confirmation before wiping
    #[arg(long)]
    ask: bool,
  },

  /// Immediately expire all entries with a TTL
  Expire {
    /// Ask for confirmation before expiring
    #[arg(long)]
    ask: bool,
  },

  /// Optimize database using VACUUM
  Vacuum,

  /// Show database statistics
  Stats,
}

fn confirm(prompt: &str) -> bool {
  Confirm::new(prompt)
    .with_default(false)
    .prompt()
    .unwrap_or_else(|e| {
      log::error!("confirmation prompt failed: {e}");
      false
    })
}

#[expect(
  clippy::too_many_lines,
  reason = "single-binary command dispatch is still clearer here"
)]
fn main() -> eyre::Result<()> {
  color_eyre::install()?;

  // Check if we're being called as a multicall binary
  //
  // NOTE: We cannot use clap's multicall here because it requires the main
  // command to have no arguments (only subcommands), but our Cli has global
  // arguments like --max-items, --db-path, etc. Instead, we manually detect
  // the invocation name and route appropriately. While this is ugly, it's
  // seemingly the only option.
  let program_name = env::args().next().map(|s| {
    PathBuf::from(s)
      .file_name()
      .and_then(|name| name.to_str())
      .unwrap_or("stash")
      .to_string()
  });

  if let Some(ref name) = program_name {
    if name == "wl-copy" || name == "stash-copy" {
      crate::multicall::wl_copy::wl_copy_main()?;
      return Ok(());
    } else if name == "wl-paste" || name == "stash-paste" {
      crate::multicall::wl_paste::wl_paste_main()?;
      return Ok(());
    }
  }

  // Normal CLI handling
  smol::block_on(async {
    let cli = Cli::parse();
    let global_ask = cli.ask;
    env_logger::Builder::new()
      .filter_level(cli.verbosity.into())
      .init();

    let db_path = match cli.db_path {
      Some(path) => path,
      None => {
        let cache_dir = dirs::cache_dir().ok_or_else(|| {
          eyre::eyre!(
            "could not determine cache directory. set --db-path or \
             $STASH_DB_PATH explicitly"
          )
        })?;
        cache_dir.join("stash").join("db")
      },
    };

    if let Some(parent) = db_path.parent() {
      std::fs::create_dir_all(parent)?;
    }

    let conn = rusqlite::Connection::open(&db_path)?;
    let db = db::SqliteClipboardDb::new(conn, db_path)?;

    match cli.command {
      Some(Command::Store) => {
        let state = env::var("STASH_CLIPBOARD_STATE").ok();
        db.store(
          io::stdin(),
          cli.max_dedupe_search,
          cli.max_items,
          state,
          #[cfg(feature = "use-toplevel")]
          &cli.excluded_apps,
          #[cfg(not(feature = "use-toplevel"))]
          &[],
          cli.min_size,
          cli.max_size,
        )?;
      },
      Some(Command::List {
        format,
        expired,
        reverse,
      }) => {
        match format.as_deref() {
          Some("tsv") => {
            db.list(io::stdout(), cli.preview_width, expired, reverse)?;
          },
          Some("json") => {
            println!("{}", db.list_json(expired, reverse)?);
          },
          Some(other) => {
            bail!("unsupported format: {other}");
          },
          None => {
            if std::io::stdout().is_terminal() {
              db.list_tui(cli.preview_width, expired, reverse)?;
            } else {
              db.list(io::stdout(), cli.preview_width, expired, reverse)?;
            }
          },
        }
      },
      Some(Command::Decode { input }) => {
        db.decode(io::stdin(), io::stdout(), input)?;
      },
      Some(Command::Delete { arg, r#type, ask }) => {
        let mut should_proceed = true;
        if global_ask || ask {
          should_proceed =
            confirm("Are you sure you want to delete clipboard entries?");

          if !should_proceed {
            log::info!("aborted by user");
          }
        }
        if should_proceed {
          match (arg, r#type.as_deref()) {
            (Some(s), Some("id")) => {
              let id = s
                .parse::<u64>()
                .map_err(|_| eyre::eyre!("argument is not a valid id"))?;
              use std::io::Cursor;
              db.delete(Cursor::new(format!("{id}\n")))?;
            },
            (Some(s), Some("query")) => {
              db.query_delete(&s)?;
            },
            (Some(s), None) => {
              if let Ok(id) = s.parse::<u64>() {
                use std::io::Cursor;
                db.delete(Cursor::new(format!("{id}\n")))?;
              } else {
                db.query_delete(&s)?;
              }
            },
            (None, _) => {
              db.delete(io::stdin())?;
            },
            (_, Some(_)) => {
              bail!("unknown type for --type. use \"id\" or \"query\"");
            },
          }
        }
      },

      Some(Command::Db { action }) => {
        match action {
          DbAction::Wipe { expired, ask } => {
            let mut should_proceed = true;
            if global_ask || ask {
              let message = if expired {
                "Are you sure you want to wipe all expired clipboard entries?"
              } else {
                "Are you sure you want to wipe ALL clipboard history?"
              };
              should_proceed = confirm(message);
              if !should_proceed {
                log::info!("db wipe command aborted by user");
              }
            }
            if should_proceed {
              if expired {
                match db.cleanup_expired() {
                  Ok(count) => {
                    log::info!("wiped {count} expired entries");
                  },
                  Err(e) => {
                    return Err(e.into());
                  },
                }
              } else {
                db.wipe_db()?;
              }
            }
          },
          DbAction::Expire { ask } => {
            let should_proceed = !(global_ask || ask)
              || confirm(
                "Are you sure you want to immediately expire all entries with \
                 a TTL?",
              );
            if should_proceed {
              match db.expire_ttl_entries() {
                Ok(0) => {
                  println!("no entries with a TTL to expire");
                },
                Ok(count) => {
                  println!("marked {count} entries as expired");
                },
                Err(e) => {
                  return Err(e.into());
                },
              }
            } else {
              log::info!("db expire command aborted by user");
            }
          },
          DbAction::Vacuum => {
            db.vacuum()?;
            log::info!("database optimized successfully");
          },
          DbAction::Stats => {
            println!("{}", db.stats()?);
          },
        }
      },

      Some(Command::Import { r#type, ask }) => {
        let mut should_proceed = true;
        if global_ask || ask {
          should_proceed = confirm(
            "Are you sure you want to import clipboard data? This may \
             overwrite existing entries.",
          );
          if !should_proceed {
            log::info!("import command aborted by user");
          }
        }
        if should_proceed {
          let format = r#type.as_deref().unwrap_or("tsv");
          match format {
            "tsv" => {
              ImportCommand::import_tsv(&db, io::stdin(), cli.max_items)?;
            },
            _ => {
              bail!("unsupported import format: {format}");
            },
          }
        }
      },
      Some(Command::Watch {
        expire_after,
        mime_type,
        persist,
      }) => {
        db.watch(
          cli.max_dedupe_search,
          cli.max_items,
          #[cfg(feature = "use-toplevel")]
          &cli.excluded_apps,
          #[cfg(not(feature = "use-toplevel"))]
          &[],
          expire_after,
          &mime_type,
          cli.min_size,
          cli.max_size,
          persist,
        )
        .await;
      },

      None => {
        Cli::command().print_help()?;
        println!();
      },
    }
    Ok(())
  })
}
