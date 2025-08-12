use crate::db::{ClipboardDb, SledClipboardDb};

use crate::db::StashError;

pub trait WipeCommand {
    fn wipe(&self) -> Result<(), StashError>;
}

impl WipeCommand for SledClipboardDb {
    fn wipe(&self) -> Result<(), StashError> {
        self.wipe_db()?;
        log::info!("Database wiped");
        Ok(())
    }
}
