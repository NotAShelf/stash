use std::{
    env,
    io::{self},
    path::PathBuf,
    process,
};

use clap::{CommandFactory, Parser, Subcommand};

mod commands;
mod db;
mod import;

use crate::commands::decode::DecodeCommand;
use crate::commands::delete::DeleteCommand;
use crate::commands::list::ListCommand;
use crate::commands::query::QueryCommand;
use crate::commands::store::StoreCommand;
use crate::commands::watch::WatchCommand;
use crate::commands::wipe::WipeCommand;
use crate::import::ImportCommand;

#[derive(Parser)]
#[command(name = "stash")]
#[command(about = "Wayland clipboard manager", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long, default_value_t = 750)]
    max_items: u64,

    #[arg(long, default_value_t = 100)]
    max_dedupe_search: u64,

    #[arg(long, default_value_t = 100)]
    preview_width: u32,

    #[arg(long)]
    db_path: Option<PathBuf>,

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

    /// Delete clipboard entry by id (if numeric), or entries matching a query (if not).
    /// Numeric arguments are treated as ids. Use --type to specify explicitly.
    Delete {
        /// Id or query string
        arg: Option<String>,

        /// Explicitly specify type: "id" or "query"
        #[arg(long, value_parser = ["id", "query"])]
        r#type: Option<String>,
    },

    /// Wipe all clipboard history
    Wipe,

    /// Import clipboard data from stdin (default: TSV format)
    Import {
        /// Explicitly specify format: "tsv" (default)
        #[arg(long, value_parser = ["tsv"])]
        r#type: Option<String>,
    },

    /// Watch clipboard for changes and store automatically
    Watch,
}

fn report_error<T>(result: Result<T, impl std::fmt::Display>, context: &str) -> Option<T> {
    match result {
        Ok(val) => Some(val),
        Err(e) => {
            log::error!("{context}: {e}");
            None
        }
    }
}

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
            }
        };

        match cli.command {
            Some(Command::Store) => {
                let state = env::var("STASH_CLIPBOARD_STATE").ok();
                report_error(
                    db.store(io::stdin(), cli.max_dedupe_search, cli.max_items, state),
                    "Failed to store entry",
                );
            }
            Some(Command::List { format }) => {
                let format = format.as_deref().unwrap_or("tsv");
                match format {
                    "tsv" => {
                        report_error(
                            db.list(io::stdout(), cli.preview_width),
                            "Failed to list entries",
                        );
                    }
                    "json" => {
                        // Implement JSON output
                        match db.list_json() {
                            Ok(json) => {
                                println!("{json}");
                            }
                            Err(e) => {
                                log::error!("Failed to list entries as JSON: {e}");
                            }
                        }
                    }
                    _ => {
                        log::error!("Unsupported format: {format}");
                    }
                }
            }
            Some(Command::Decode { input }) => {
                report_error(
                    db.decode(io::stdin(), io::stdout(), input),
                    "Failed to decode entry",
                );
            }
            Some(Command::Delete { arg, r#type }) => match (arg, r#type.as_deref()) {
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
                }
                (Some(s), Some("query")) => {
                    report_error(db.query_delete(&s), "Failed to delete entry by query");
                }
                (Some(s), None) => {
                    if let Ok(id) = s.parse::<u64>() {
                        use std::io::Cursor;
                        report_error(
                            db.delete(Cursor::new(format!("{id}\n"))),
                            "Failed to delete entry by id",
                        );
                    } else {
                        report_error(db.query_delete(&s), "Failed to delete entry by query");
                    }
                }
                (None, _) => {
                    report_error(db.delete(io::stdin()), "Failed to delete entry from stdin");
                }
                (_, Some(_)) => {
                    log::error!("Unknown type for --type. Use \"id\" or \"query\".");
                }
            },
            Some(Command::Wipe) => {
                report_error(db.wipe(), "Failed to wipe database");
            }

            Some(Command::Import { r#type }) => {
                // Default format is TSV (Cliphist compatible)
                let format = r#type.as_deref().unwrap_or("tsv");
                match format {
                    "tsv" => {
                        db.import_tsv(io::stdin());
                    }
                    _ => {
                        log::error!("Unsupported import format: {format}");
                    }
                }
            }
            Some(Command::Watch) => {
                db.watch(cli.max_dedupe_search, cli.max_items);
            }
            None => {
                if let Err(e) = Cli::command().print_help() {
                    eprintln!("Failed to print help: {e}");
                }
                println!();
            }
        }
    });
}
