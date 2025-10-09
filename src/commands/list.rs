use std::io::Write;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

pub trait ListCommand {
  fn list(&self, out: impl Write, preview_width: u32)
  -> Result<(), StashError>;
}

impl ListCommand for SqliteClipboardDb {
  fn list(
    &self,
    out: impl Write,
    preview_width: u32,
  ) -> Result<(), StashError> {
    self.list_entries(out, preview_width).map(|_| ())
  }
}

impl SqliteClipboardDb {
  #[allow(clippy::too_many_lines)]
  pub fn list_tui(&self, preview_width: u32) -> Result<(), StashError> {
    use std::io::stdout;

    use crossterm::{
      event::{
        self,
        DisableMouseCapture,
        EnableMouseCapture,
        Event,
        KeyCode,
        KeyModifiers,
      },
      execute,
      terminal::{
        EnterAlternateScreen,
        LeaveAlternateScreen,
        disable_raw_mode,
        enable_raw_mode,
      },
    };
    use notify_rust::Notification;
    use ratatui::{
      Terminal,
      backend::CrosstermBackend,
      style::{Color, Modifier, Style},
      text::{Line, Span},
      widgets::{Block, Borders, List, ListItem, ListState},
    };
    use wl_clipboard_rs::copy::{MimeType, Options, Source};

    // Query entries from DB
    let mut stmt = self
      .conn
      .prepare("SELECT id, contents, mime FROM clipboard ORDER BY id DESC")
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    let mut rows = stmt
      .query([])
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    let mut entries: Vec<(u64, String, String)> = Vec::new();
    let mut max_id_width = 2;
    let mut max_mime_width = 8;
    while let Some(row) = rows
      .next()
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?
    {
      let id: u64 = row
        .get(0)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let contents: Vec<u8> = row
        .get(1)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let mime: Option<String> = row
        .get(2)
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
      let preview =
        crate::db::preview_entry(&contents, mime.as_deref(), preview_width);
      let mime_str = mime.as_deref().unwrap_or("").to_string();
      let id_str = id.to_string();
      max_id_width = max_id_width.max(id_str.width());
      max_mime_width = max_mime_width.max(mime_str.width());
      entries.push((id, preview, mime_str));
    }

    enable_raw_mode()
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    let mut state = ListState::default();
    if !entries.is_empty() {
      state.select(Some(0));
    }

    let res = (|| -> Result<(), StashError> {
      loop {
        terminal
          .draw(|f| {
            let area = f.area();
            let block = Block::default()
              .title(
                "Clipboard Entries (j/k/↑/↓ to move, Enter to copy, Shift+D \
                 to delete, q/ESC to quit)",
              )
              .borders(Borders::ALL);

            let border_width = 2;
            let highlight_symbol = ">";
            let highlight_width = 1;
            let content_width = area.width as usize - border_width;

            // Minimum widths for columns
            let min_id_width = 2;
            let min_mime_width = 6;
            let min_preview_width = 4;
            let spaces = 3; // [id][ ][preview][ ][mime]

            // Dynamically allocate widths
            let mut id_col = max_id_width.max(min_id_width);
            let mut mime_col = max_mime_width.max(min_mime_width);
            let mut preview_col = content_width
              .saturating_sub(highlight_width)
              .saturating_sub(id_col)
              .saturating_sub(mime_col)
              .saturating_sub(spaces);

            // If not enough space, shrink columns
            if preview_col < min_preview_width {
              let needed = min_preview_width - preview_col;
              if mime_col > min_mime_width {
                let reduce = mime_col - min_mime_width;
                let take = reduce.min(needed);
                mime_col -= take;
                preview_col += take;
              }
            }
            if preview_col < min_preview_width {
              let needed = min_preview_width - preview_col;
              if id_col > min_id_width {
                let reduce = id_col - min_id_width;
                let take = reduce.min(needed);
                id_col -= take;
                preview_col += take;
              }
            }
            if preview_col < min_preview_width {
              preview_col = min_preview_width;
            }

            let selected = state.selected();

            let list_items: Vec<ListItem> = entries
              .iter()
              .enumerate()
              .map(|(i, entry)| {
                // Truncate preview by grapheme clusters and display width
                let mut preview = String::new();
                let mut width = 0;
                for g in entry.1.graphemes(true) {
                  let g_width = UnicodeWidthStr::width(g);
                  if width + g_width > preview_col {
                    preview.push('…');
                    break;
                  }
                  preview.push_str(g);
                  width += g_width;
                }
                // Truncate and pad mimetype
                let mut mime = String::new();
                let mut mwidth = 0;
                for g in entry.2.graphemes(true) {
                  let g_width = UnicodeWidthStr::width(g);
                  if mwidth + g_width > mime_col {
                    mime.push('…');
                    break;
                  }
                  mime.push_str(g);
                  mwidth += g_width;
                }

                // Compose the row as highlight + id + space + preview + space +
                // mimetype
                let mut spans = Vec::new();
                let (id, preview, mime) = entry;
                if Some(i) == selected {
                  spans.push(Span::styled(
                    highlight_symbol,
                    Style::default()
                      .fg(Color::Yellow)
                      .add_modifier(Modifier::BOLD),
                  ));
                  spans.push(Span::styled(
                    format!("{id:>id_col$}"),
                    Style::default()
                      .fg(Color::Yellow)
                      .add_modifier(Modifier::BOLD),
                  ));
                  spans.push(Span::raw(" "));
                  spans.push(Span::styled(
                    format!("{preview:<preview_col$}"),
                    Style::default()
                      .fg(Color::Yellow)
                      .add_modifier(Modifier::BOLD),
                  ));
                  spans.push(Span::raw(" "));
                  spans.push(Span::styled(
                    format!("{mime:>mime_col$}"),
                    Style::default().fg(Color::Green),
                  ));
                } else {
                  spans.push(Span::raw(" "));
                  spans.push(Span::raw(format!("{id:>id_col$}")));
                  spans.push(Span::raw(" "));
                  spans.push(Span::raw(format!("{preview:<preview_col$}")));
                  spans.push(Span::raw(" "));
                  spans.push(Span::raw(format!("{mime:>mime_col$}")));
                }
                ListItem::new(Line::from(spans))
              })
              .collect();

            let list = List::new(list_items)
              .block(block)
              .highlight_style(
                Style::default()
                  .fg(Color::Yellow)
                  .add_modifier(Modifier::BOLD),
              )
              .highlight_symbol(""); // handled manually

            f.render_stateful_widget(list, area, &mut state);
          })
          .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

        if event::poll(std::time::Duration::from_millis(250))
          .map_err(|e| StashError::ListDecode(e.to_string().into()))?
        {
          if let Event::Key(key) = event::read()
            .map_err(|e| StashError::ListDecode(e.to_string().into()))?
          {
            match (key.code, key.modifiers) {
              (KeyCode::Char('q') | KeyCode::Esc, _) => break,
              (KeyCode::Down | KeyCode::Char('j'), _) => {
                let i = match state.selected() {
                  Some(i) => {
                    if i >= entries.len() - 1 {
                      0
                    } else {
                      i + 1
                    }
                  },
                  None => 0,
                };
                state.select(Some(i));
              },
              (KeyCode::Up | KeyCode::Char('k'), _) => {
                let i = match state.selected() {
                  Some(i) => {
                    if i == 0 {
                      entries.len() - 1
                    } else {
                      i - 1
                    }
                  },
                  None => 0,
                };
                state.select(Some(i));
              },
              (KeyCode::Enter, _) => {
                if let Some(idx) = state.selected() {
                  if let Some((id, ..)) = entries.get(idx) {
                    // Fetch full contents for the selected entry
                    let (contents, mime): (Vec<u8>, Option<String>) = self
                      .conn
                      .query_row(
                        "SELECT contents, mime FROM clipboard WHERE id = ?1",
                        rusqlite::params![id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                      )
                      .map_err(|e| {
                        StashError::ListDecode(e.to_string().into())
                      })?;
                    // Copy to clipboard
                    let opts = Options::new();
                    // Default clipboard is regular, seat is default
                    let mime_type = match mime {
                      Some(ref m) if m == "text/plain" => MimeType::Text,
                      Some(ref m) => MimeType::Specific(m.clone()),
                      None => MimeType::Text,
                    };
                    let copy_result = opts
                      .copy(Source::Bytes(contents.clone().into()), mime_type);
                    match copy_result {
                      Ok(()) => {
                        let _ = Notification::new()
                          .summary("Stash")
                          .body("Copied entry to clipboard")
                          .show();
                      },
                      Err(e) => {
                        log::error!("Failed to copy entry to clipboard: {e}");
                        let _ = Notification::new()
                          .summary("Stash")
                          .body(&format!("Failed to copy to clipboard: {e}"))
                          .show();
                      },
                    }
                  }
                }
              },
              (KeyCode::Char('D'), KeyModifiers::SHIFT) => {
                if let Some(idx) = state.selected() {
                  if let Some((id, ..)) = entries.get(idx) {
                    // Delete entry from DB
                    self
                      .conn
                      .execute(
                        "DELETE FROM clipboard WHERE id = ?1",
                        rusqlite::params![id],
                      )
                      .map_err(|e| {
                        StashError::DeleteEntry(*id, e.to_string().into())
                      })?;
                    // Remove from entries and update selection
                    entries.remove(idx);
                    let new_len = entries.len();
                    if new_len == 0 {
                      state.select(None);
                    } else if idx >= new_len {
                      state.select(Some(new_len - 1));
                    } else {
                      state.select(Some(idx));
                    }
                    // Show notification
                    let _ = Notification::new()
                      .summary("Stash")
                      .body("Deleted entry")
                      .show();
                  }
                }
              },
              _ => {},
            }
          }
        }
      }
      Ok(())
    })();

    // Ignore errors during terminal restore, as we can't recover here.
    let _ = disable_raw_mode();
    let _ = execute!(
      terminal.backend_mut(),
      LeaveAlternateScreen,
      DisableMouseCapture
    );
    let _ = terminal.show_cursor();

    res
  }
}
