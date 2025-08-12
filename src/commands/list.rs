use crate::db::{ClipboardDb, SledClipboardDb};
use std::io::Write;

pub trait ListCommand {
    fn list(&self, out: impl Write, preview_width: u32) -> Result<(), crate::db::StashError>;
}

impl ListCommand for SledClipboardDb {
    fn list(&self, out: impl Write, preview_width: u32) -> Result<(), crate::db::StashError> {
        self.list_entries(out, preview_width)?;
        log::info!("Listed clipboard entries");
        Ok(())
    }
}
