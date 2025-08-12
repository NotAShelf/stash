use crate::db::{ClipboardDb, SledClipboardDb};

use crate::db::StashError;

pub trait QueryCommand {
    fn query_delete(&self, query: &str) -> Result<usize, StashError>;
}

impl QueryCommand for SledClipboardDb {
    fn query_delete(&self, query: &str) -> Result<usize, StashError> {
        <SledClipboardDb as ClipboardDb>::delete_query(self, query)
    }
}
