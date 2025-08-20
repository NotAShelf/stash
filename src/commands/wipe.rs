use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

pub trait WipeCommand {
  fn wipe(&self) -> Result<(), StashError>;
}

impl WipeCommand for SqliteClipboardDb {
  fn wipe(&self) -> Result<(), StashError> {
    self.wipe_db()?;
    log::info!("Database wiped");
    Ok(())
  }
}
