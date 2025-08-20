use std::{
  env,
  io::{self},
  path::PathBuf,
  process,
};

use atty::Stream;
use clap::{CommandFactory, Parser, Subcommand};
use inquire::Confirm;

mod commands;
mod db;

use crate::commands::{
  decode::DecodeCommand,
  delete::DeleteCommand,
  import::ImportCommand,
  list::ListCommand,
  query::QueryCommand,
  store::StoreCommand,
  watch::WatchCommand,
  wipe::WipeCommand,
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
  #[arg(long, default_value_t = 100)]
  max_dedupe_search: u64,

  /// Maximum width (in characters) for clipboard entry previews in list
  /// output.
  #[arg(long, default_value_t = 100)]
  preview_width: u32,

  /// Path to the SQLite clipboard database file.
  #[arg(long)]
  db_path: Option<PathBuf>,

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
  Wipe {
    /// Ask for confirmation before wiping
    #[arg(long)]
    ask: bool,
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

  /// Watch clipboard for changes and store automatically
  Watch,
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
fn main() {
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
      if let Err(e) = std::fs::create_dir_all(parent) {
        log::error!("Failed to create database directory: {e}");
        process::exit(1);
      }
    }

    let conn = rusqlite::Connection::open(&db_path).unwrap_or_else(|e| {
      log::error!("Failed to open SQLite database: {e}");
      process::exit(1);
    });

    let db = match db::SqliteClipboardDb::new(conn) {
      Ok(db) => db,
      Err(e) => {
        log::error!("Failed to initialize SQLite database: {e}");
        process::exit(1);
      },
    };

    match cli.command {
      Some(Command::Store) => {
        let state = env::var("STASH_CLIPBOARD_STATE").ok();
        report_error(
          db.store(io::stdin(), cli.max_dedupe_search, cli.max_items, state),
          "Failed to store entry",
        );
      },
      Some(Command::List { format }) => {
        match format.as_deref() {
          Some("tsv") => {
            report_error(
              db.list(io::stdout(), cli.preview_width),
              "Failed to list entries",
            );
          },
          Some("json") => {
            match db.list_json() {
              Ok(json) => {
                println!("{json}");
              },
              Err(e) => {
                log::error!("Failed to list entries as JSON: {e}");
              },
            }
          },
          Some(other) => {
            log::error!("Unsupported format: {other}");
          },
          None => {
            if atty::is(Stream::Stdout) {
              report_error(
                db.list_tui(cli.preview_width),
                "Failed to list entries in TUI",
              );
            } else {
              report_error(
                db.list(io::stdout(), cli.preview_width),
                "Failed to list entries",
              );
            }
          },
        }
      },
      Some(Command::Decode { input }) => {
        report_error(
          db.decode(io::stdin(), io::stdout(), input),
          "Failed to decode entry",
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
            log::info!("Aborted by user.");
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
                log::error!("Argument is not a valid id");
              }
            },
            (Some(s), Some("query")) => {
              report_error(
                db.query_delete(&s),
                "Failed to delete entry by query",
              );
            },
            (Some(s), None) => {
              if let Ok(id) = s.parse::<u64>() {
                use std::io::Cursor;
                report_error(
                  db.delete(Cursor::new(format!("{id}\n"))),
                  "Failed to delete entry by id",
                );
              } else {
                report_error(
                  db.query_delete(&s),
                  "Failed to delete entry by query",
                );
              }
            },
            (None, _) => {
              report_error(
                db.delete(io::stdin()),
                "Failed to delete entry from stdin",
              );
            },
            (_, Some(_)) => {
              log::error!("Unknown type for --type. Use \"id\" or \"query\".");
            },
          }
        }
      },
      Some(Command::Wipe { ask }) => {
        let mut should_proceed = true;
        if ask {
          should_proceed = Confirm::new(
            "Are you sure you want to wipe all clipboard history?",
          )
          .with_default(false)
          .prompt()
          .unwrap_or(false);
          if !should_proceed {
            log::info!("Aborted by user.");
          }
        }
        if should_proceed {
          report_error(db.wipe(), "Failed to wipe database");
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
            log::info!("Aborted by user.");
          }
        }
        if should_proceed {
          let format = r#type.as_deref().unwrap_or("tsv");
          match format {
            "tsv" => {
              if let Err(e) =
                ImportCommand::import_tsv(&db, io::stdin(), cli.max_items)
              {
                log::error!("Failed to import TSV: {e}");
              }
            },
            _ => {
              log::error!("Unsupported import format: {format}");
            },
          }
        }
      },
      Some(Command::Watch) => {
        db.watch(cli.max_dedupe_search, cli.max_items);
      },
      None => {
        if let Err(e) = Cli::command().print_help() {
          log::error!("Failed to print help: {e}");
        }
        println!();
      },
    }
  });
}
