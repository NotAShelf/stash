use crate::db::{ClipboardDb, SqliteClipboardDb, StashError};

pub trait QueryCommand {
  fn query_delete(&self, query: &str) -> Result<usize, StashError>;
}

impl QueryCommand for SqliteClipboardDb {
  fn query_delete(&self, query: &str) -> Result<usize, StashError> {
    <Self as ClipboardDb>::delete_query(self, query)
  }
}
