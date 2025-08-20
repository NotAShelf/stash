use std::io::{self, BufRead};

use crate::db::{
  ClipboardDb,
  Entry,
  SqliteClipboardDb,
  StashError,
  detect_mime,
};

pub trait ImportCommand {
  /// Import clipboard entries from TSV format.
  fn import_tsv(
    &self,
    input: impl io::Read,
    max_items: u64,
  ) -> Result<(), StashError>;
}

impl ImportCommand for SqliteClipboardDb {
  fn import_tsv(
    &self,
    input: impl io::Read,
    max_items: u64,
  ) -> Result<(), StashError> {
    let reader = io::BufReader::new(input);
    let mut imported = 0;
    for (lineno, line) in reader.lines().enumerate() {
      let line = line.map_err(|e| {
        StashError::Store(format!("Failed to read line {lineno}: {e}"))
      })?;
      let mut parts = line.splitn(2, '\t');
      let (Some(id_str), Some(val)) = (parts.next(), parts.next()) else {
        return Err(StashError::Store(format!(
          "Malformed TSV line {lineno}: {line:?}"
        )));
      };

      let Ok(_id) = id_str.parse::<u64>() else {
        return Err(StashError::Store(format!(
          "Failed to parse id from line {lineno}: {id_str}"
        )));
      };

      let entry = Entry {
        contents: val.as_bytes().to_vec(),
        mime:     detect_mime(val.as_bytes()),
      };

      self
        .conn
        .execute(
          "INSERT INTO clipboard (contents, mime) VALUES (?1, ?2)",
          rusqlite::params![entry.contents, entry.mime],
        )
        .map_err(|e| {
          StashError::Store(format!(
            "Failed to insert entry at line {lineno}: {e}"
          ))
        })?;
      imported += 1;
    }

    log::info!("Imported {imported} records from TSV into SQLite database.");

    // Trim database to max_items after import
    self.trim_db(max_items)?;
    log::info!("Trimmed clipboard database to max_items = {max_items}");

    Ok(())
  }
}
