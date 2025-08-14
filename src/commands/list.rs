use std::io::Write;

use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};
use unicode_width::UnicodeWidthStr;

pub trait ListCommand {
    fn list(&self, out: impl Write, preview_width: u32) -> Result<(), StashError>;
}

impl ListCommand for SqliteClipboardDb {
    fn list(&self, out: impl Write, preview_width: u32) -> Result<(), StashError> {
        self.list_entries(out, preview_width)?;
        log::info!("Listed clipboard entries");
        Ok(())
    }
}

impl SqliteClipboardDb {
    /// Public TUI listing function for use in main.rs
    #[allow(clippy::too_many_lines)]
    pub fn list_tui(&self, preview_width: u32) -> Result<(), StashError> {
        use crossterm::{
            event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
            execute,
            terminal::{
                EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
            },
        };
        use ratatui::{
            Terminal,
            backend::CrosstermBackend,
            style::{Color, Modifier, Style},
            text::{Line, Span},
            widgets::{Block, Borders, List, ListItem, ListState},
        };
        use std::io::stdout;

        // Query entries from DB
        let mut stmt = self
            .conn
            .prepare("SELECT id, contents, mime FROM clipboard ORDER BY id DESC")
            .map_err(|e| StashError::ListDecode(e.to_string()))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| StashError::ListDecode(e.to_string()))?;

        struct EntryRow {
            id: u64,
            preview: String,
            mime: String,
        }

        let mut entries: Vec<EntryRow> = Vec::new();
        let mut max_id_width = 2;
        let mut max_mime_width = 8;
        while let Some(row) = rows
            .next()
            .map_err(|e| StashError::ListDecode(e.to_string()))?
        {
            let id: u64 = row
                .get(0)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let contents: Vec<u8> = row
                .get(1)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let mime: Option<String> = row
                .get(2)
                .map_err(|e| StashError::ListDecode(e.to_string()))?;
            let preview = crate::db::preview_entry(&contents, mime.as_deref(), preview_width);
            let mime_str = mime.as_deref().unwrap_or("").to_string();
            let id_str = id.to_string();
            max_id_width = max_id_width.max(id_str.width());
            max_mime_width = max_mime_width.max(mime_str.width());
            entries.push(EntryRow {
                id,
                preview,
                mime: mime_str,
            });
        }

        enable_raw_mode().map_err(|e| StashError::ListDecode(e.to_string()))?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
            .map_err(|e| StashError::ListDecode(e.to_string()))?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal =
            Terminal::new(backend).map_err(|e| StashError::ListDecode(e.to_string()))?;

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
                            .title("Clipboard Entries (j/k/↑/↓ to move, q/ESC to quit)")
                            .borders(Borders::ALL);

                        use unicode_segmentation::UnicodeSegmentation;
                        use unicode_width::UnicodeWidthStr;

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
                                for g in entry.preview.graphemes(true) {
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
                                for g in entry.mime.graphemes(true) {
                                    let g_width = UnicodeWidthStr::width(g);
                                    if mwidth + g_width > mime_col {
                                        mime.push('…');
                                        break;
                                    }
                                    mime.push_str(g);
                                    mwidth += g_width;
                                }

                                let preview_str = format!("{preview:<preview_col$}");
                                let mime_str = format!("{mime:>mime_col$}");

                                // Compose the row as highlight + id + space + preview + space + mimetype
                                let mut spans = Vec::new();
                                if Some(i) == selected {
                                    spans.push(Span::styled(
                                        highlight_symbol,
                                        Style::default()
                                            .fg(Color::Yellow)
                                            .add_modifier(Modifier::BOLD),
                                    ));
                                    spans.push(Span::styled(
                                        format!("{:>width$}", entry.id, width = id_col),
                                        Style::default()
                                            .fg(Color::Yellow)
                                            .add_modifier(Modifier::BOLD),
                                    ));
                                    spans.push(Span::raw(" "));
                                    spans.push(Span::styled(
                                        preview_str,
                                        Style::default()
                                            .fg(Color::Yellow)
                                            .add_modifier(Modifier::BOLD),
                                    ));
                                    spans.push(Span::raw(" "));
                                    spans.push(Span::styled(
                                        mime_str,
                                        Style::default().fg(Color::Green),
                                    ));
                                } else {
                                    spans.push(Span::raw(" "));
                                    spans.push(Span::raw(format!(
                                        "{:>width$}",
                                        entry.id,
                                        width = id_col
                                    )));
                                    spans.push(Span::raw(" "));
                                    spans.push(Span::raw(preview_str));
                                    spans.push(Span::raw(" "));
                                    spans.push(Span::raw(mime_str));
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
                    .map_err(|e| StashError::ListDecode(e.to_string()))?;

                if event::poll(std::time::Duration::from_millis(250))
                    .map_err(|e| StashError::ListDecode(e.to_string()))?
                {
                    if let Event::Key(key) =
                        event::read().map_err(|e| StashError::ListDecode(e.to_string()))?
                    {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Down | KeyCode::Char('j') => {
                                let i = match state.selected() {
                                    Some(i) => {
                                        if i >= entries.len() - 1 {
                                            0
                                        } else {
                                            i + 1
                                        }
                                    }
                                    None => 0,
                                };
                                state.select(Some(i));
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                let i = match state.selected() {
                                    Some(i) => {
                                        if i == 0 {
                                            entries.len() - 1
                                        } else {
                                            i - 1
                                        }
                                    }
                                    None => 0,
                                };
                                state.select(Some(i));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(())
        })();

        disable_raw_mode().ok();
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )
        .ok();
        terminal.show_cursor().ok();

        res
    }
}
