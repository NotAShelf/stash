use crate::db::{ClipboardDb, SledClipboardDb};
use std::io::Read;

pub trait StoreCommand {
    fn store(
        &self,
        input: impl Read,
        max_dedupe_search: u64,
        max_items: u64,
        state: Option<String>,
    );
}

impl StoreCommand for SledClipboardDb {
    fn store(
        &self,
        input: impl Read,
        max_dedupe_search: u64,
        max_items: u64,
        state: Option<String>,
    ) {
        match state.as_deref() {
            Some("sensitive") | Some("clear") => {
                self.delete_last();
            }
            _ => {
                self.store_entry(input, max_dedupe_search, max_items);
            }
        }
    }
}
