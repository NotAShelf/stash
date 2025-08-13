use crate::db::{ClipboardDb, SqliteClipboardDb};

use std::io::{Read, Write};

use crate::db::StashError;
use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};

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
        mut in_: impl Read,
        mut out: impl Write,
        input: Option<String>,
    ) -> Result<(), StashError> {
        let input_str = if let Some(s) = input {
            s
        } else {
            let mut buf = String::new();
            if let Err(e) = in_.read_to_string(&mut buf) {
                log::error!("Failed to read stdin for decode: {e}");
            }
            buf
        };

        // If input is empty or whitespace, treat as error and trigger fallback
        if input_str.trim().is_empty() {
            log::info!("No input provided to decode; relaying clipboard to stdout");
            if let Ok((mut reader, _mime)) =
                get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Any)
            {
                let mut buf = Vec::new();
                if let Err(err) = reader.read_to_end(&mut buf) {
                    log::error!("Failed to read clipboard for relay: {err}");
                } else {
                    let _ = out.write_all(&buf);
                }
            } else {
                log::error!("Failed to get clipboard contents for relay");
            }
            return Ok(());
        }

        // Try decode as usual
        match self.decode_entry(input_str.as_bytes(), &mut out, Some(input_str.clone())) {
            Ok(()) => {
                log::info!("Entry decoded");
            }
            Err(e) => {
                log::error!("Failed to decode entry: {e}");
                if let Ok((mut reader, _mime)) =
                    get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Any)
                {
                    let mut buf = Vec::new();
                    if let Err(err) = reader.read_to_end(&mut buf) {
                        log::error!("Failed to read clipboard for relay: {err}");
                    } else {
                        let _ = out.write_all(&buf);
                    }
                } else {
                    log::error!("Failed to get clipboard contents for relay");
                }
            }
        }
        Ok(())
    }
}
