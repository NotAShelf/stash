use crate::db::{ClipboardDb, SledClipboardDb};
use std::io::Write;

pub trait ListCommand {
    fn list(&self, out: impl Write, preview_width: u32);
}

impl ListCommand for SledClipboardDb {
    fn list(&self, out: impl Write, preview_width: u32) {
        self.list_entries(out, preview_width);
    }
}
