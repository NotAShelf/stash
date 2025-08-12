use crate::{Entry, detect_mime, u64_to_ivec};
use rmp_serde::encode::to_vec;
use sled::Db;
use std::io::{self, BufRead};

pub fn import_tsv(db: &Db, input: impl io::Read) {
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
                let enc = to_vec(&entry).unwrap();
                db.insert(u64_to_ivec(id), enc).unwrap();
                imported += 1;
            }
        }
    }
    eprintln!("Imported {imported} records from TSV into sled database.");
}
