use crate::db::{ClipboardDb, SledClipboardDb};

use std::io::{Read, Write};

pub trait DecodeCommand {
    fn decode(&self, in_: impl Read, out: impl Write, input: Option<String>);
}

impl DecodeCommand for SledClipboardDb {
    fn decode(&self, in_: impl Read, out: impl Write, input: Option<String>) {
        self.decode_entry(in_, out, input);
        log::info!("Entry decoded");
    }
}
