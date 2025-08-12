use crate::db::{ClipboardDb, SqliteClipboardDb};

use std::io::{Read, Write};

use crate::db::StashError;

pub trait DecodeCommand {
    fn decode(
        &self,
        in_: impl Read,
        out: impl Write,
        input: Option<String>,
    ) -> Result<(), StashError>;
}

impl DecodeCommand for SqliteClipboardDb {
    fn decode(
        &self,
        in_: impl Read,
        out: impl Write,
        input: Option<String>,
    ) -> Result<(), StashError> {
        self.decode_entry(in_, out, input)?;
        log::info!("Entry decoded");
        Ok(())
    }
}
