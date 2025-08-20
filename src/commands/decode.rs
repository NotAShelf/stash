use std::io::{Read, Write};

use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};

use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

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
      in_
        .read_to_string(&mut buf)
        .map_err(|e| StashError::DecodeRead(e.to_string()))?;
      buf
    };

    // If input is empty or whitespace, treat as error and trigger fallback
    if input_str.trim().is_empty() {
      log::debug!("No input provided to decode; relaying clipboard to stdout");
      if let Ok((mut reader, _mime)) =
        get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Any)
      {
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).map_err(|e| {
          StashError::DecodeRead(format!(
            "Failed to read clipboard for relay: {e}"
          ))
        })?;
        out.write_all(&buf).map_err(|e| {
          StashError::DecodeWrite(format!(
            "Failed to write clipboard relay: {e}"
          ))
        })?;
      } else {
        return Err(StashError::DecodeGet(
          "Failed to get clipboard contents for relay".to_string(),
        ));
      }
      return Ok(());
    }

    // Try decode as usual
    match self.decode_entry(
      input_str.as_bytes(),
      &mut out,
      Some(input_str.clone()),
    ) {
      Ok(()) => Ok(()),
      Err(e) => {
        // On decode failure, relay clipboard as fallback
        if let Ok((mut reader, _mime)) =
          get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Any)
        {
          let mut buf = Vec::new();
          reader.read_to_end(&mut buf).map_err(|err| {
            StashError::DecodeRead(format!(
              "Failed to read clipboard for relay: {err}"
            ))
          })?;
          out.write_all(&buf).map_err(|err| {
            StashError::DecodeWrite(format!(
              "Failed to write clipboard relay: {err}"
            ))
          })?;
          Ok(())
        } else {
          Err(e)
        }
      },
    }
  }
}
