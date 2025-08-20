use std::io::Read;

use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

pub trait DeleteCommand {
  fn delete(&self, input: impl Read) -> Result<usize, StashError>;
}

impl DeleteCommand for SqliteClipboardDb {
  fn delete(&self, input: impl Read) -> Result<usize, StashError> {
    match self.delete_entries(input) {
      Ok(deleted) => {
        log::info!("Deleted {deleted} entries");
        Ok(deleted)
      },
      Err(e) => {
        log::error!("Failed to delete entries: {e}");
        Err(e)
      },
    }
  }
}
