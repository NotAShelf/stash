use crate::db::{Entry, SledClipboardDb, detect_mime, u64_to_ivec};
use log::{error, info};
use std::io::{self, BufRead};

pub trait ImportCommand {
    fn import_tsv(&self, input: impl io::Read);
}

impl ImportCommand for SledClipboardDb {
    fn import_tsv(&self, input: impl io::Read) {
        let reader = io::BufReader::new(input);
        let mut imported = 0;
        for line in reader.lines().map_while(Result::ok) {
            let mut parts = line.splitn(2, '\t');
            if let (Some(id_str), Some(val)) = (parts.next(), parts.next()) {
                if let Ok(id) = id_str.parse::<u64>() {
                    let entry = Entry {
                        contents: val.as_bytes().to_vec(),
                        mime: detect_mime(val.as_bytes()),
                    };
                    let enc = match rmp_serde::encode::to_vec(&entry) {
                        Ok(enc) => enc,
                        Err(e) => {
                            error!("Failed to encode entry for id {id}: {e}");
                            continue;
                        }
                    };
                    match self.db.insert(u64_to_ivec(id), enc) {
                        Ok(_) => {
                            imported += 1;
                            info!("Imported entry with id {id}");
                        }
                        Err(e) => error!("Failed to insert entry with id {id}: {e}"),
                    }
                } else {
                    error!("Failed to parse id from line: {id_str}");
                }
            } else {
                error!("Malformed TSV line: {line:?}");
            }
        }
        info!("Imported {imported} records from TSV into sled database.");
    }
}
