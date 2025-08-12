use crate::db::{Entry, SqliteClipboardDb, detect_mime};
use log::{error, info};
use std::io::{self, BufRead};

pub trait ImportCommand {
    fn import_tsv(&self, input: impl io::Read);
}

impl ImportCommand for SqliteClipboardDb {
    fn import_tsv(&self, input: impl io::Read) {
        let reader = io::BufReader::new(input);
        let mut imported = 0;
        for line in reader.lines().map_while(Result::ok) {
            let mut parts = line.splitn(2, '\t');
            if let (Some(id_str), Some(val)) = (parts.next(), parts.next()) {
                if let Ok(_id) = id_str.parse::<u64>() {
                    let entry = Entry {
                        contents: val.as_bytes().to_vec(),
                        mime: detect_mime(val.as_bytes()),
                    };
                    match self.conn.execute(
                        "INSERT INTO clipboard (contents, mime) VALUES (?1, ?2)",
                        rusqlite::params![entry.contents, entry.mime],
                    ) {
                        Ok(_) => {
                            imported += 1;
                            info!("Imported entry from TSV");
                        }
                        Err(e) => error!("Failed to insert entry: {e}"),
                    }
                } else {
                    error!("Failed to parse id from line: {id_str}");
                }
            } else {
                error!("Malformed TSV line: {line:?}");
            }
        }
        info!("Imported {imported} records from TSV into SQLite database.");
    }
}
