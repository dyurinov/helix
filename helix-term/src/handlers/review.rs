use helix_event::register_hook;
use helix_view::events::{DocumentDidClose, DocumentDidOpen};

use crate::commands::review::{document_closed, sync_document};

pub(super) fn register_hooks() {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        // Show review threads in documents opened while a review is active.
        if event.editor.review.is_some() {
            sync_document(event.editor, event.doc);
        }
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        if event.editor.review.is_some() {
            document_closed(event.editor, &event.doc);
        }
        Ok(())
    });
}
