use std::{
    env,
    io::{self},
    path::PathBuf,
    process,
};

use clap::{Parser, Subcommand};

mod commands;
mod db;
mod import;

use crate::commands::decode::DecodeCommand;
use crate::commands::delete::DeleteCommand;
use crate::commands::list::ListCommand;
use crate::commands::query::QueryCommand;
use crate::commands::store::StoreCommand;
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
    List,

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
}

fn main() {
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

    let sled_db = sled::open(&db_path).unwrap_or_else(|e| {
        log::error!("Failed to open database: {e}");
        process::exit(1);
    });

    let db = db::SledClipboardDb { db: sled_db };

    match cli.command {
        Some(Command::Store) => {
            let state = env::var("STASH_CLIPBOARD_STATE").ok();
            if let Err(e) = db.store(io::stdin(), cli.max_dedupe_search, cli.max_items, state) {
                log::error!("Failed to store entry: {e}");
            }
        }
        Some(Command::List) => {
            if let Err(e) = db.list(io::stdout(), cli.preview_width) {
                log::error!("Failed to list entries: {e}");
            }
        }
        Some(Command::Decode { input }) => {
            if let Err(e) = db.decode(io::stdin(), io::stdout(), input) {
                log::error!("Failed to decode entry: {e}");
            }
        }
        Some(Command::Delete { arg, r#type }) => match (arg, r#type.as_deref()) {
            (Some(s), Some("id")) => {
                if let Ok(id) = s.parse::<u64>() {
                    use std::io::Cursor;
                    if let Err(e) = db.delete(Cursor::new(format!("{id}\n"))) {
                        log::error!("Failed to delete entry by id: {e}");
                    }
                } else {
                    log::error!("Argument is not a valid id");
                }
            }
            (Some(s), Some("query")) => {
                if let Err(e) = db.query_delete(&s) {
                    log::error!("Failed to delete entry by query: {e}");
                }
            }
            (Some(s), None) => {
                if let Ok(id) = s.parse::<u64>() {
                    use std::io::Cursor;
                    if let Err(e) = db.delete(Cursor::new(format!("{id}\n"))) {
                        log::error!("Failed to delete entry by id: {e}");
                    }
                } else {
                    if let Err(e) = db.query_delete(&s) {
                        log::error!("Failed to delete entry by query: {e}");
                    }
                }
            }
            (None, _) => {
                if let Err(e) = db.delete(io::stdin()) {
                    log::error!("Failed to delete entry from stdin: {e}");
                }
            }
            (_, Some(_)) => {
                log::error!("Unknown type for --type. Use \"id\" or \"query\".");
            }
        },
        Some(Command::Wipe) => {
            if let Err(e) = db.wipe() {
                log::error!("Failed to wipe database: {e}");
            }
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
        _ => {
            log::warn!("No subcommand provided");
        }
    }
}
