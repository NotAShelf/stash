use crate::db::{ClipboardDb, SledClipboardDb};

pub trait WipeCommand {
    fn wipe(&self);
}

impl WipeCommand for SledClipboardDb {
    fn wipe(&self) {
        self.wipe_db();
        log::info!("Database wiped");
    }
}
