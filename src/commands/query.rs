use crate::db::{ClipboardDb, SqliteClipboardDb};

use crate::db::StashError;

pub trait QueryCommand {
    fn query_delete(&self, query: &str) -> Result<usize, StashError>;
}

impl QueryCommand for SqliteClipboardDb {
    fn query_delete(&self, query: &str) -> Result<usize, StashError> {
        <SqliteClipboardDb as ClipboardDb>::delete_query(self, query)
    }
}
