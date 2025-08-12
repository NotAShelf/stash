use crate::db::{ClipboardDb, SledClipboardDb};
use std::io::Read;

pub trait DeleteCommand {
    fn delete(&self, input: impl Read);
}

impl DeleteCommand for SledClipboardDb {
    fn delete(&self, input: impl Read) {
        self.delete_entries(input);
    }
}
