use crate::db::{ClipboardDb, SqliteClipboardDb};

use std::io::Read;

pub trait StoreCommand {
    fn store(
        &self,
        input: impl Read,
        max_dedupe_search: u64,
        max_items: u64,
        state: Option<String>,
    ) -> Result<(), crate::db::StashError>;
}

impl StoreCommand for SqliteClipboardDb {
    fn store(
        &self,
        input: impl Read,
        max_dedupe_search: u64,
        max_items: u64,
        state: Option<String>,
    ) -> Result<(), crate::db::StashError> {
        if let Some("sensitive" | "clear") = state.as_deref() {
            self.delete_last()?;
            log::info!("Entry deleted");
        } else {
            self.store_entry(input, max_dedupe_search, max_items)?;
            log::info!("Entry stored");
        }
        Ok(())
    }
}
