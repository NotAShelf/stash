use std::io::Read;

use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

pub trait DeleteCommand {
  fn delete(&self, input: impl Read) -> Result<usize, StashError>;
}

impl DeleteCommand for SqliteClipboardDb {
  fn delete(&self, input: impl Read) -> Result<usize, StashError> {
    let deleted = self.delete_entries(input)?;
    log::info!("Deleted {deleted} entries");
    Ok(deleted)
  }
}
