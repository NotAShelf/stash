use std::{
  env,
  io::{self, IsTerminal},
  path::PathBuf,
  time::Duration,
};

use clap::{CommandFactory, Parser, Subcommand};
use humantime::parse_duration;
use inquire::Confirm;

mod commands;
pub(crate) mod db;
pub(crate) mod mime;
mod multicall;
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
    wipe::WipeCommand,
  },
  db::DEFAULT_MAX_ENTRY_SIZE,
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

  /// Wipe all clipboard history
  ///
  /// DEPRECATED: Use `stash db wipe` instead
  #[command(hide = true)]
  Wipe {
    /// Ask for confirmation before wiping
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

  /// Optimize database using VACUUM
  Vacuum,

  /// Show database statistics
  Stats,
}

fn report_error<T>(
  result: Result<T, impl std::fmt::Display>,
  context: &str,
) -> Option<T> {
  match result {
    Ok(val) => Some(val),
    Err(e) => {
      log::error!("{context}: {e}");
      None
    },
  }
}

#[allow(clippy::too_many_lines)] // whatever
fn main() -> color_eyre::eyre::Result<()> {
  // Check if we're being called as a multicall binary
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
    env_logger::Builder::new()
      .filter_level(cli.verbosity.into())
      .init();

    let db_path = cli.db_path.unwrap_or_else(|| {
      dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("stash")
        .join("db")
    });

    if let Some(parent) = db_path.parent() {
      std::fs::create_dir_all(parent)?;
    }

    let conn = rusqlite::Connection::open(&db_path)?;
    let db = db::SqliteClipboardDb::new(conn)?;

    match cli.command {
      Some(Command::Store) => {
        let state = env::var("STASH_CLIPBOARD_STATE").ok();
        report_error(
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
          ),
          "failed to store entry",
        );
      },
      Some(Command::List { format, expired }) => {
        match format.as_deref() {
          Some("tsv") => {
            report_error(
              db.list(io::stdout(), cli.preview_width, expired),
              "failed to list entries",
            );
          },
          Some("json") => {
            match db.list_json(expired) {
              Ok(json) => {
                println!("{json}");
              },
              Err(e) => {
                log::error!("failed to list entries as JSON: {e}");
              },
            }
          },
          Some(other) => {
            log::error!("unsupported format: {other}");
          },
          None => {
            if std::io::stdout().is_terminal() {
              report_error(
                db.list_tui(cli.preview_width, expired),
                "failed to list entries in TUI",
              );
            } else {
              report_error(
                db.list(io::stdout(), cli.preview_width, expired),
                "failed to list entries",
              );
            }
          },
        }
      },
      Some(Command::Decode { input }) => {
        report_error(
          db.decode(io::stdin(), io::stdout(), input),
          "failed to decode entry",
        );
      },
      Some(Command::Delete { arg, r#type, ask }) => {
        let mut should_proceed = true;
        if ask {
          should_proceed =
            Confirm::new("Are you sure you want to delete clipboard entries?")
              .with_default(false)
              .prompt()
              .unwrap_or(false);

          if !should_proceed {
            log::info!("aborted by user.");
          }
        }
        if should_proceed {
          match (arg, r#type.as_deref()) {
            (Some(s), Some("id")) => {
              if let Ok(id) = s.parse::<u64>() {
                use std::io::Cursor;
                report_error(
                  db.delete(Cursor::new(format!("{id}\n"))),
                  "Failed to delete entry by id",
                );
              } else {
                log::error!("argument is not a valid id");
              }
            },
            (Some(s), Some("query")) => {
              report_error(
                db.query_delete(&s),
                "failed to delete entry by query",
              );
            },
            (Some(s), None) => {
              if let Ok(id) = s.parse::<u64>() {
                use std::io::Cursor;
                report_error(
                  db.delete(Cursor::new(format!("{id}\n"))),
                  "failed to delete entry by id",
                );
              } else {
                report_error(
                  db.query_delete(&s),
                  "failed to delete entry by query",
                );
              }
            },
            (None, _) => {
              report_error(
                db.delete(io::stdin()),
                "failed to delete entry from stdin",
              );
            },
            (_, Some(_)) => {
              log::error!("unknown type for --type. Use \"id\" or \"query\".");
            },
          }
        }
      },
      Some(Command::Wipe { ask }) => {
        eprintln!(
          "Warning: The 'stash wipe' command is deprecated. Use 'stash db \
           wipe' instead."
        );
        let mut should_proceed = true;
        if ask {
          should_proceed = Confirm::new(
            "Are you sure you want to wipe all clipboard history?",
          )
          .with_default(false)
          .prompt()
          .unwrap_or(false);
          if !should_proceed {
            log::info!("wipe command aborted by user.");
          }
        }
        if should_proceed {
          report_error(db.wipe(), "failed to wipe database");
        }
      },

      Some(Command::Db { action }) => {
        match action {
          DbAction::Wipe { expired, ask } => {
            let mut should_proceed = true;
            if ask {
              let message = if expired {
                "Are you sure you want to wipe all expired clipboard entries?"
              } else {
                "Are you sure you want to wipe ALL clipboard history?"
              };
              should_proceed = Confirm::new(message)
                .with_default(false)
                .prompt()
                .unwrap_or(false);
              if !should_proceed {
                log::info!("db wipe command aborted by user.");
              }
            }
            if should_proceed {
              if expired {
                match db.cleanup_expired() {
                  Ok(count) => {
                    log::info!("Wiped {} expired entries", count);
                  },
                  Err(e) => {
                    log::error!("failed to wipe expired entries: {e}");
                  },
                }
              } else {
                report_error(db.wipe(), "failed to wipe database");
              }
            }
          },
          DbAction::Vacuum => {
            match db.vacuum() {
              Ok(()) => {
                log::info!("Database optimized successfully");
              },
              Err(e) => {
                log::error!("failed to vacuum database: {e}");
              },
            }
          },
          DbAction::Stats => {
            match db.stats() {
              Ok(stats) => {
                println!("{}", stats);
              },
              Err(e) => {
                log::error!("failed to get database stats: {e}");
              },
            }
          },
        }
      },

      Some(Command::Import { r#type, ask }) => {
        let mut should_proceed = true;
        if ask {
          should_proceed = Confirm::new(
            "Are you sure you want to import clipboard data? This may \
             overwrite existing entries.",
          )
          .with_default(false)
          .prompt()
          .unwrap_or(false);
          if !should_proceed {
            log::info!("import command aborted by user.");
          }
        }
        if should_proceed {
          let format = r#type.as_deref().unwrap_or("tsv");
          match format {
            "tsv" => {
              if let Err(e) =
                ImportCommand::import_tsv(&db, io::stdin(), cli.max_items)
              {
                log::error!("failed to import TSV: {e}");
              }
            },
            _ => {
              log::error!("unsupported import format: {format}");
            },
          }
        }
      },
      Some(Command::Watch {
        expire_after,
        mime_type,
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
        );
      },

      None => {
        Cli::command().print_help()?;
        println!();
      },
    }
    Ok(())
  })
}
