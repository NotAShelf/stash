use std::{
    env, fmt,
    io::{self, BufRead, BufReader, Read, Write},
    path::PathBuf,
    process, str,
};

use clap::{Parser, Subcommand};
use image::{GenericImageView, ImageFormat};
use rmp_serde::{decode::from_read, encode::to_vec};
use serde::{Deserialize, Serialize};
use sled::{Db, IVec};

mod import;

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
    #[arg(long)]
    import_tsv: bool,
}

#[derive(Subcommand)]
enum Command {
    Store,
    List,
    Decode { input: Option<String> },
    DeleteQuery { query: String },
    Delete,
    Wipe,
    Import,
}

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub contents: Vec<u8>,
    pub mime: Option<String>,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let preview = preview_entry(&self.contents, self.mime.as_deref(), 100);
        write!(f, "{preview}")
    }
}

fn main() {
    let cli = Cli::parse();

    let db_path = cli.db_path.unwrap_or_else(|| {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("stash")
            .join("db")
    });

    let db = sled::open(&db_path).unwrap_or_else(|e| {
        eprintln!("Failed to open database: {e}");
        process::exit(1);
    });

    if cli.import_tsv {
        import::import_tsv(&db, io::stdin());
        return;
    }

    match cli.command {
        Some(Command::Store) => {
            let state = env::var("CLIPBOARD_STATE").unwrap_or_default();
            match state.as_str() {
                "sensitive" | "clear" => {
                    delete_last(&db);
                }
                _ => {
                    store_entry(&db, io::stdin(), cli.max_dedupe_search, cli.max_items);
                }
            }
        }
        Some(Command::List) => {
            list_entries(&db, io::stdout(), cli.preview_width);
        }
        Some(Command::Decode { input }) => {
            decode_entry(&db, io::stdin(), io::stdout(), input);
        }
        Some(Command::DeleteQuery { query }) => {
            delete_query(&db, &query);
        }
        Some(Command::Delete) => {
            delete_entries(&db, io::stdin());
        }
        Some(Command::Wipe) => {
            wipe_db(&db);
        }
        Some(Command::Import) => {
            eprintln!("Use --import-tsv to import TSV clipboard data");
        }
        _ => {
            eprintln!("No subcommand provided");
        }
    }
}

fn store_entry(db: &Db, mut input: impl Read, max_dedupe_search: u64, max_items: u64) {
    let mut buf = Vec::new();
    if input.read_to_end(&mut buf).is_err() || buf.is_empty() || buf.len() > 5 * 1_000_000 {
        return;
    }
    if buf.iter().all(|b| b.is_ascii_whitespace()) {
        return;
    }

    let mime = detect_mime(&buf);

    deduplicate(db, &buf, max_dedupe_search);

    let entry = Entry {
        contents: buf.clone(),
        mime,
    };

    let id = next_sequence(db);
    let enc = to_vec(&entry).unwrap();

    db.insert(u64_to_ivec(id), enc).unwrap();
    trim_db(db, max_items);
}

fn deduplicate(db: &Db, buf: &[u8], max: u64) {
    let mut count = 0;
    for item in db.iter().rev().take(max as usize) {
        let (k, v) = item.unwrap();
        let entry: Entry = from_read(v.as_ref()).unwrap();
        if entry.contents == buf {
            db.remove(k).unwrap();
        }
        count += 1;
        if count >= max {
            break;
        }
    }
}

fn trim_db(db: &Db, max: u64) {
    let mut keys: Vec<_> = db.iter().rev().map(|kv| kv.unwrap().0).collect();
    if keys.len() as u64 > max {
        for k in keys.drain((max as usize)..) {
            db.remove(k).unwrap();
        }
    }
}

fn delete_last(db: &Db) {
    if let Some((k, _)) = db.iter().next_back().and_then(Result::ok) {
        db.remove(k).unwrap();
    }
}

fn wipe_db(db: &Db) {
    db.clear().unwrap();
}

fn list_entries(db: &Db, mut out: impl Write, preview_width: u32) {
    for (k, v) in db.iter().rev().filter_map(Result::ok) {
        let id = ivec_to_u64(&k);
        let entry: Entry = from_read(v.as_ref()).unwrap();
        let preview = preview_entry(&entry.contents, entry.mime.as_deref(), preview_width);
        writeln!(out, "{id}\t{preview}").unwrap();
    }
}

fn decode_entry(db: &Db, mut in_: impl Read, mut out: impl Write, input: Option<String>) {
    let s = if let Some(input) = input {
        input
    } else {
        let mut buf = String::new();
        in_.read_to_string(&mut buf).unwrap();
        buf
    };
    let id = extract_id(&s).unwrap();
    let v = db.get(u64_to_ivec(id)).unwrap().unwrap();
    let entry: Entry = from_read(v.as_ref()).unwrap();
    out.write_all(&entry.contents).unwrap();
}

fn delete_query(db: &Db, query: &str) {
    for (k, v) in db.iter().filter_map(Result::ok) {
        let entry: Entry = from_read(v.as_ref()).unwrap();
        if entry
            .contents
            .windows(query.len())
            .any(|w| w == query.as_bytes())
        {
            db.remove(k).unwrap();
        }
    }
}

fn delete_entries(db: &Db, in_: impl Read) {
    let reader = BufReader::new(in_);
    for line in reader.lines().map_while(Result::ok) {
        if let Ok(id) = extract_id(&line) {
            db.remove(u64_to_ivec(id)).unwrap();
        }
    }
}

fn extract_id(input: &str) -> Result<u64, &'static str> {
    let id_str = input.split('\t').next().unwrap_or("");
    id_str.parse().map_err(|_| "invalid id")
}

fn next_sequence(db: &Db) -> u64 {
    let last = db
        .iter()
        .next_back()
        .and_then(|r| r.ok())
        .map(|(k, _)| ivec_to_u64(&k));
    last.unwrap_or(0) + 1
}

fn u64_to_ivec(v: u64) -> IVec {
    IVec::from(&v.to_be_bytes()[..])
}

fn ivec_to_u64(v: &IVec) -> u64 {
    let arr: [u8; 8] = v.as_ref().try_into().unwrap();
    u64::from_be_bytes(arr)
}

fn detect_mime(data: &[u8]) -> Option<String> {
    if image::guess_format(data).is_ok() {
        match image::guess_format(data) {
            Ok(fmt) => Some(
                match fmt {
                    ImageFormat::Png => "image/png",
                    ImageFormat::Jpeg => "image/jpeg",
                    ImageFormat::Gif => "image/gif",
                    ImageFormat::Bmp => "image/bmp",
                    ImageFormat::Tiff => "image/tiff",
                    _ => "application/octet-stream",
                }
                .to_string(),
            ),
            Err(_) => None,
        }
    } else if data.is_ascii() {
        Some("text/plain".into())
    } else {
        None
    }
}

fn preview_entry(data: &[u8], mime: Option<&str>, width: u32) -> String {
    if let Some(mime) = mime {
        if mime.starts_with("image/") {
            if let Ok(img) = image::load_from_memory(data) {
                let (w, h) = img.dimensions();
                return format!(
                    "[[ binary data {} {} {}x{} ]]",
                    size_str(data.len()),
                    mime,
                    w,
                    h
                );
            }
        } else if mime == "application/json" || mime.starts_with("text/") {
            let s = str::from_utf8(data).unwrap_or("");
            let s = s.trim().replace(|c: char| c.is_whitespace(), " ");
            return truncate(&s, width as usize, "…");
        }
    }
    let s = String::from_utf8_lossy(data);
    truncate(s.trim(), width as usize, "…")
}

fn truncate(s: &str, max: usize, ellip: &str) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect::<String>() + ellip
    } else {
        s.to_string()
    }
}

fn size_str(size: usize) -> String {
    let units = ["B", "KiB", "MiB"];
    let mut fsize = size as f64;
    let mut i = 0;
    while fsize >= 1024.0 && i < units.len() - 1 {
        fsize /= 1024.0;
        i += 1;
    }
    format!("{:.0} {}", fsize, units[i])
}
