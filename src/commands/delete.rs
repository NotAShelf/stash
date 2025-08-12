use crate::db::{ClipboardDb, SledClipboardDb, StashError};

use std::io::Read;

pub trait DeleteCommand {
    fn delete(&self, input: impl Read) -> Result<usize, StashError>;
}

impl DeleteCommand for SledClipboardDb {
    fn delete(&self, input: impl Read) -> Result<usize, StashError> {
        match self.delete_entries(input) {
            Ok(deleted) => {
                log::info!("Deleted {deleted} entries");
                Ok(deleted)
            }
            Err(e) => {
                log::error!("Failed to delete entries: {e}");
                Err(e)
            }
        }
    }
}
