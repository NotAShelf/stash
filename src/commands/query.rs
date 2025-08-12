use crate::db::{ClipboardDb, SledClipboardDb};

pub trait QueryCommand {
    fn query_delete(&self, query: &str);
}

impl QueryCommand for SledClipboardDb {
    fn query_delete(&self, query: &str) {
        <SledClipboardDb as ClipboardDb>::delete_query(self, query);
    }
}
