use std::io::{self, BufRead};

use log::{error, info};

use crate::db::{
  ClipboardDb,
  Entry,
  SqliteClipboardDb,
  StashError,
  detect_mime,
};

pub trait ImportCommand {
  /// Import clipboard entries from TSV format.
  ///
  /// # Arguments
  ///
  /// * `input` - A readable stream containing TSV lines, each of the form
  ///   `<id>\t<contents>`.
  /// * `max_items` - The maximum number of clipboard entries to keep after
  ///   import. If set to `u64::MAX`, no trimming occurs.
  ///
  /// # Returns
  ///
  /// * `Ok(())` if all entries are imported and trimming succeeds.
  /// * `Err(StashError)` if any error occurs during import or trimming.
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
    for line in reader.lines().map_while(Result::ok) {
      let mut parts = line.splitn(2, '\t');
      let (Some(id_str), Some(val)) = (parts.next(), parts.next()) else {
        error!("Malformed TSV line: {line:?}");
        continue;
      };

      let Ok(_id) = id_str.parse::<u64>() else {
        error!("Failed to parse id from line: {id_str}");
        continue;
      };

      let entry = Entry {
        contents: val.as_bytes().to_vec(),
        mime:     detect_mime(val.as_bytes()),
      };

      match self.conn.execute(
        "INSERT INTO clipboard (contents, mime) VALUES (?1, ?2)",
        rusqlite::params![entry.contents, entry.mime],
      ) {
        Ok(_) => {
          imported += 1;
          info!("Imported entry from TSV");
        },
        Err(e) => error!("Failed to insert entry: {e}"),
      }
    }
    info!("Imported {imported} records from TSV into SQLite database.");

    // Trim database to max_items after import
    self.trim_db(max_items)?;
    info!("Trimmed clipboard database to max_items = {max_items}");
    Ok(())
  }
}
