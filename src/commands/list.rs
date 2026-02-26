use std::io::Write;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

pub trait ListCommand {
  fn list(
    &self,
    out: impl Write,
    preview_width: u32,
    include_expired: bool,
  ) -> Result<(), StashError>;
}

impl ListCommand for SqliteClipboardDb {
  fn list(
    &self,
    out: impl Write,
    preview_width: u32,
    include_expired: bool,
  ) -> Result<(), StashError> {
    self
      .list_entries(out, preview_width, include_expired)
      .map(|_| ())
  }
}

/// All mutable state for the TUI list view.
struct TuiState {
  /// Total number of entries matching the current filter in the DB.
  total: usize,

  /// Global cursor position: index into the full ordered result set.
  cursor: usize,

  /// DB offset of `window[0]`, i.e., the first row currently loaded.
  viewport_offset: usize,

  /// The loaded slice of entries: `(id, preview, mime)`.
  window: Vec<(i64, String, String)>,

  /// How many rows the window holds (== visible list height).
  window_size: usize,

  /// Whether the window needs to be re-fetched from the DB.
  dirty: bool,

  /// Current search query. Empty string means no filter.
  search_query: String,

  /// Whether we're currently in search input mode.
  search_mode: bool,
}

impl TuiState {
  /// Create initial state: count total rows, load the first window.
  fn new(
    db: &SqliteClipboardDb,
    include_expired: bool,
    window_size: usize,
    preview_width: u32,
  ) -> Result<Self, StashError> {
    let total = db.count_entries(include_expired, None)?;
    let window = if total > 0 {
      db.fetch_entries_window(
        include_expired,
        0,
        window_size,
        preview_width,
        None,
      )?
    } else {
      Vec::new()
    };
    Ok(Self {
      total,
      cursor: 0,
      viewport_offset: 0,
      window,
      window_size,
      dirty: false,
      search_query: String::new(),
      search_mode: false,
    })
  }

  /// Return the current search filter (`None` if empty).
  fn search_filter(&self) -> Option<&str> {
    if self.search_query.is_empty() {
      None
    } else {
      Some(&self.search_query)
    }
  }

  /// Update search query and reset cursor. Returns true if search changed.
  fn set_search(&mut self, query: String) -> bool {
    let changed = self.search_query != query;
    if changed {
      self.search_query = query;
      self.cursor = 0;
      self.viewport_offset = 0;
      self.dirty = true;
    }
    changed
  }

  /// Clear search and reset state. Returns true if was searching.
  fn clear_search(&mut self) -> bool {
    let had_search = !self.search_query.is_empty();
    self.search_query.clear();
    self.search_mode = false;
    if had_search {
      self.cursor = 0;
      self.viewport_offset = 0;
      self.dirty = true;
    }
    had_search
  }

  /// Toggle search mode.
  fn toggle_search_mode(&mut self) {
    self.search_mode = !self.search_mode;
    if self.search_mode {
      // When entering search mode, clear query if there was one
      // or start fresh
      self.search_query.clear();
      self.dirty = true;
    }
  }

  /// Return the cursor position relative to the current window
  /// (`window[local_cursor]` == the selected entry).
  #[inline]
  fn local_cursor(&self) -> usize {
    self.cursor.saturating_sub(self.viewport_offset)
  }

  /// Return the selected `(id, preview, mime)` if any entry is selected.
  fn selected_entry(&self) -> Option<&(i64, String, String)> {
    if self.total == 0 {
      return None;
    }
    self.window.get(self.local_cursor())
  }

  /// Move the cursor down by one, wrapping to 0 at the bottom.
  fn move_down(&mut self) {
    if self.total == 0 {
      return;
    }
    self.cursor = if self.cursor + 1 >= self.total {
      0
    } else {
      self.cursor + 1
    };
    self.dirty = true;
  }

  /// Move the cursor up by one, wrapping to `total - 1` at the top.
  fn move_up(&mut self) {
    if self.total == 0 {
      return;
    }
    self.cursor = if self.cursor == 0 {
      self.total - 1
    } else {
      self.cursor - 1
    };
    self.dirty = true;
  }

  /// Resize the window (e.g. terminal resized).  Marks dirty so the
  /// viewport is reloaded on the next frame.
  fn resize(&mut self, new_size: usize) {
    if new_size != self.window_size {
      self.window_size = new_size;
      self.dirty = true;
    }
  }

  /// After a delete the total shrinks by one and the cursor may need
  /// clamping.  The caller is responsible for the DB deletion itself.
  fn on_delete(&mut self) {
    if self.total == 0 {
      return;
    }
    self.total -= 1;
    if self.total == 0 {
      self.cursor = 0;
    } else if self.cursor >= self.total {
      self.cursor = self.total - 1;
    }
    self.dirty = true;
  }

  /// Reload the window from the DB if `dirty` is set or if the cursor
  /// has drifted outside the currently loaded range.
  fn sync(
    &mut self,
    db: &SqliteClipboardDb,
    include_expired: bool,
    preview_width: u32,
  ) -> Result<(), StashError> {
    let cursor_out_of_window = self.cursor < self.viewport_offset
      || self.cursor >= self.viewport_offset + self.window.len().max(1);

    if !self.dirty && !cursor_out_of_window {
      return Ok(());
    }

    // Re-anchor the viewport so the cursor sits in the upper half when
    // scrolling downward, or at a sensible position when wrapping.
    let half = self.window_size / 2;
    self.viewport_offset = if self.cursor >= half {
      (self.cursor - half).min(self.total.saturating_sub(self.window_size))
    } else {
      0
    };

    let search = self.search_filter();
    self.window = if self.total > 0 {
      db.fetch_entries_window(
        include_expired,
        self.viewport_offset,
        self.window_size,
        preview_width,
        search,
      )?
    } else {
      Vec::new()
    };
    self.dirty = false;
    Ok(())
  }
}

/// Query the maximum id digit-width and maximum mime byte-length across
/// all entries. This is pretty damn fast as it touches only index/metadata,
/// not blobs.
fn global_column_widths(
  db: &SqliteClipboardDb,
  include_expired: bool,
) -> Result<(usize, usize), StashError> {
  let filter = if include_expired {
    ""
  } else {
    "WHERE (is_expired IS NULL OR is_expired = 0)"
  };
  let query = format!(
    "SELECT COALESCE(MAX(LENGTH(CAST(id AS TEXT))), 2), \
     COALESCE(MAX(LENGTH(mime)), 8) FROM clipboard {filter}"
  );
  let (id_w, mime_w): (i64, i64) = db
    .conn
    .query_row(&query, [], |r| Ok((r.get(0)?, r.get(1)?)))
    .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
  Ok((id_w.max(2) as usize, mime_w.max(8) as usize))
}

impl SqliteClipboardDb {
  #[allow(clippy::too_many_lines)]
  pub fn list_tui(
    &self,
    preview_width: u32,
    include_expired: bool,
  ) -> Result<(), StashError> {
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

    // One-time column-width metadata (no blob reads).
    let (max_id_width, max_mime_width) =
      global_column_widths(self, include_expired)?;

    enable_raw_mode()
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
      .map_err(|e| StashError::ListDecode(e.to_string().into()))?;

    // Derive initial window size from current terminal height.
    let initial_height = terminal
      .size()
      .map(|r| r.height.saturating_sub(2) as usize)
      .unwrap_or(24);
    let initial_height = initial_height.max(1);

    let mut tui =
      TuiState::new(self, include_expired, initial_height, preview_width)?;

    // ratatui ListState; only tracks selection within the *window* slice.
    let mut list_state = ListState::default();
    if tui.total > 0 {
      list_state.select(Some(0));
    }

    /// Accumulated actions from draining the event queue.
    struct EventActions {
      quit:             bool,
      net_down:         i64, // positive=down, negative=up, 0=none
      copy:             bool,
      delete:           bool,
      toggle_search:    bool, // enter/exit search mode
      search_input:     Option<char>, // character typed in search mode
      search_backspace: bool, // backspace in search mode
      clear_search:     bool, // clear search query (ESC in search mode)
    }

    /// Drain all pending key events and return what actions to perform.
    /// Navigation is capped to +-1 per frame to prevent jumpy scrolling when
    /// the key-repeat rate exceeds the render frame rate.
    fn drain_events(tui: &TuiState) -> Result<EventActions, StashError> {
      let mut actions = EventActions {
        quit:             false,
        net_down:         0,
        copy:             false,
        delete:           false,
        toggle_search:    false,
        search_input:     None,
        search_backspace: false,
        clear_search:     false,
      };

      while event::poll(std::time::Duration::from_millis(0))
        .map_err(|e| StashError::ListDecode(e.to_string().into()))?
      {
        if let Event::Key(key) = event::read()
          .map_err(|e| StashError::ListDecode(e.to_string().into()))?
        {
          if tui.search_mode {
            // In search mode, handle text input
            match (key.code, key.modifiers) {
              (KeyCode::Esc, _) => {
                actions.clear_search = true;
              },
              (KeyCode::Enter, _) => {
                actions.toggle_search = true; // exit search mode
              },
              (KeyCode::Backspace, _) => {
                actions.search_backspace = true;
              },
              (KeyCode::Char(c), _) => {
                actions.search_input = Some(c);
              },
              _ => {},
            }
          } else {
            // Normal mode navigation commands
            match (key.code, key.modifiers) {
              (KeyCode::Char('q') | KeyCode::Esc, _) => actions.quit = true,
              (KeyCode::Down | KeyCode::Char('j'), _) => {
                // Cap at +1 per frame for smooth scrolling
                if actions.net_down < 1 {
                  actions.net_down += 1;
                }
              },
              (KeyCode::Up | KeyCode::Char('k'), _) => {
                // Cap at -1 per frame for smooth scrolling
                if actions.net_down > -1 {
                  actions.net_down -= 1;
                }
              },
              (KeyCode::Enter, _) => actions.copy = true,
              (KeyCode::Char('D'), KeyModifiers::SHIFT) => {
                actions.delete = true
              },
              (KeyCode::Char('/'), _) => actions.toggle_search = true,
              _ => {},
            }
          }
        }
      }
      Ok(actions)
    }

    let draw_frame =
      |terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
       tui: &mut TuiState,
       list_state: &mut ListState,
       max_id_width: usize,
       max_mime_width: usize|
       -> Result<(), StashError> {
        // Reserve 2 rows for search bar when in search mode
        let search_bar_height = if tui.search_mode { 2 } else { 0 };
        let term_height = terminal
          .size()
          .map(|r| r.height.saturating_sub(2 + search_bar_height) as usize)
          .unwrap_or(24)
          .max(1);
        tui.resize(term_height);
        tui.sync(self, include_expired, preview_width)?;

        if tui.total == 0 {
          list_state.select(None);
        } else {
          list_state.select(Some(tui.local_cursor()));
        }

        terminal
          .draw(|f| {
            let area = f.area();

            // Build title based on search state
            let title = if tui.search_mode {
              format!("Search: {}", tui.search_query)
            } else if tui.search_query.is_empty() {
              "Clipboard Entries (j/k/↑/↓ to move, / to search, Enter to copy, \
               Shift+D to delete, q/ESC to quit)"
                .to_string()
            } else {
              format!(
                "Clipboard Entries (filtered: '{}' - {} results, / to search, \
                 ESC to clear, q to quit)",
                tui.search_query, tui.total
              )
            };

            let block = Block::default().title(title).borders(Borders::ALL);

            let border_width = 2;
            let highlight_symbol = ">";
            let highlight_width = 1;
            let content_width = area.width as usize - border_width;

            let min_id_width = 2;
            let min_mime_width = 6;
            let min_preview_width = 4;
            let spaces = 3;

            let mut id_col = max_id_width.max(min_id_width);
            let mut mime_col = max_mime_width.max(min_mime_width);
            let mut preview_col = content_width
              .saturating_sub(highlight_width)
              .saturating_sub(id_col)
              .saturating_sub(mime_col)
              .saturating_sub(spaces);

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

            let selected = list_state.selected();

            let list_items: Vec<ListItem> = tui
              .window
              .iter()
              .enumerate()
              .map(|(i, entry)| {
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
              .highlight_symbol("");

            f.render_stateful_widget(list, area, list_state);
          })
          .map_err(|e| StashError::ListDecode(e.to_string().into()))?;
        Ok(())
      };

    // Initial draw.
    draw_frame(
      &mut terminal,
      &mut tui,
      &mut list_state,
      max_id_width,
      max_mime_width,
    )?;

    let res = (|| -> Result<(), StashError> {
      loop {
        // Block waiting for events, then drain and process all queued input.
        if event::poll(std::time::Duration::from_millis(250))
          .map_err(|e| StashError::ListDecode(e.to_string().into()))?
        {
          let actions = drain_events(&tui)?;

          if actions.quit {
            break;
          }

          // Handle search mode actions
          if actions.toggle_search {
            tui.toggle_search_mode();
          }

          if actions.clear_search && tui.clear_search() {
            // Search was cleared, refresh count
            tui.total =
              self.count_entries(include_expired, tui.search_filter())?;
          }

          if let Some(c) = actions.search_input {
            let new_query = format!("{}{}", tui.search_query, c);
            if tui.set_search(new_query) {
              // Search changed, refresh count and reset
              tui.total =
                self.count_entries(include_expired, tui.search_filter())?;
            }
          }

          if actions.search_backspace {
            let new_query = tui
              .search_query
              .chars()
              .next_back()
              .map(|_| {
                tui
                  .search_query
                  .chars()
                  .take(tui.search_query.len() - 1)
                  .collect::<String>()
              })
              .unwrap_or_default();
            if tui.set_search(new_query) {
              // Search changed, refresh count and reset
              tui.total =
                self.count_entries(include_expired, tui.search_filter())?;
            }
          }

          // Apply navigation (capped at ±1 per frame for smooth scrolling).
          if !tui.search_mode {
            if actions.net_down > 0 {
              tui.move_down();
            } else if actions.net_down < 0 {
              tui.move_up();
            }

            if actions.delete
              && let Some(&(id, ..)) = tui.selected_entry()
            {
              self
                .conn
                .execute(
                  "DELETE FROM clipboard WHERE id = ?1",
                  rusqlite::params![id],
                )
                .map_err(|e| {
                  StashError::DeleteEntry(id, e.to_string().into())
                })?;
              tui.on_delete();
              let _ = Notification::new()
                .summary("Stash")
                .body("Deleted entry")
                .show();
            }

            if actions.copy
              && let Some(&(id, ..)) = tui.selected_entry()
            {
              match self.copy_entry(id) {
                Ok((new_id, contents, mime)) => {
                  if new_id != id {
                    tui.dirty = true;
                  }
                  let opts = Options::new();
                  let mime_type = match mime {
                    Some(ref m) if m == "text/plain" => MimeType::Text,
                    Some(ref m) => MimeType::Specific(m.clone().to_owned()),
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
                },
                Err(e) => {
                  log::error!("Failed to fetch entry {id}: {e}");
                  let _ = Notification::new()
                    .summary("Stash")
                    .body(&format!("Failed to fetch entry: {e}"))
                    .show();
                },
              }
            }
          }

          // Redraw once after processing all accumulated input.
          draw_frame(
            &mut terminal,
            &mut tui,
            &mut list_state,
            max_id_width,
            max_mime_width,
          )?;
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
