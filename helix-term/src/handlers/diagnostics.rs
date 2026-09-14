use std::collections::HashSet;
use std::mem;
use std::time::Duration;
use tokio::time::Instant;

use helix_core::diagnostic::DiagnosticProvider;
use helix_core::syntax::config::LanguageServerFeature;
use helix_core::Uri;
use helix_event::{cancelable_future, register_hook, send_blocking, TaskHandle};
use helix_lsp::{lsp, LanguageServerId};
use helix_view::document::Mode;
use helix_view::events::{
    DiagnosticsDidChange, DocumentDidChange, DocumentDidOpen, DocumentFocusLost,
    LanguageServerExited, LanguageServerInitialized,
};
use helix_view::handlers::diagnostics::DiagnosticEvent;
use helix_view::handlers::lsp::{PullAllDocumentsDiagnosticsEvent, PullDiagnosticsEvent};
use helix_view::handlers::Handlers;
use helix_view::{DocumentId, Editor};

use crate::events::{OnModeSwitch, PostCommand};
use crate::job;

pub(super) fn register_hooks(handlers: &Handlers) {
    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        if event.editor.mode != Mode::Insert {
            for (view, _) in event.editor.tree.views_mut() {
                send_blocking(&view.diagnostics_handler.events, DiagnosticEvent::Refresh)
            }
        }
        Ok(())
    });
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        for (view, _) in event.cx.editor.tree.views_mut() {
            view.diagnostics_handler.active = event.new_mode != Mode::Insert;
        }
        Ok(())
    });

    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        refresh_visible_background_diagnostics(event.editor);
        Ok(())
    });
    register_hook!(move |event: &mut PostCommand<'_, '_>| {
        refresh_visible_background_diagnostics(event.cx.editor);
        Ok(())
    });
    register_hook!(move |event: &mut LanguageServerExited<'_>| {
        for doc in event.editor.documents.values_mut() {
            doc.pull_diagnostic_controllers.remove(&event.server_id);
            doc.background_diagnostic_servers.remove(&event.server_id);
        }
        Ok(())
    });

    let tx = handlers.pull_diagnostics.clone();
    let tx_all_documents = handlers.pull_all_documents_diagnostics.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event
            .doc
            .has_language_server_with_feature(LanguageServerFeature::PullDiagnostics)
            && !event.ghost_transaction
        {
            // Cancel the ongoing request, if present.
            for controller in event.doc.pull_diagnostic_controllers.values_mut() {
                controller.cancel();
            }
            let document_id = event.doc.id();
            send_blocking(&tx, PullDiagnosticsEvent { document_id });

            let inter_file_dependencies_language_servers = event
                .doc
                .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
                .filter(|language_server| {
                    language_server
                        .capabilities()
                        .diagnostic_provider
                        .as_ref()
                        .is_some_and(|diagnostic_provider| match diagnostic_provider {
                            lsp::DiagnosticServerCapabilities::Options(options) => {
                                options.inter_file_dependencies
                            }

                            lsp::DiagnosticServerCapabilities::RegistrationOptions(options) => {
                                options.diagnostic_options.inter_file_dependencies
                            }
                        })
                })
                .map(|language_server| language_server.id())
                .collect();

            send_blocking(
                &tx_all_documents,
                PullAllDocumentsDiagnosticsEvent {
                    language_servers: inter_file_dependencies_language_servers,
                    changed_document: document_id,
                },
            );
        }
        Ok(())
    });

    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        request_document_diagnostics(event.editor, event.doc);

        Ok(())
    });

    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        let doc_ids: Vec<_> = event.editor.documents.keys().copied().collect();

        for doc_id in doc_ids {
            request_document_diagnostics_for_language_servers(
                event.editor,
                doc_id,
                HashSet::from([event.server_id]),
            );
        }

        Ok(())
    });
}

#[derive(Debug, Default)]
pub(super) struct PullDiagnosticsHandler {
    document_ids: HashSet<DocumentId>,
}

impl helix_event::AsyncHook for PullDiagnosticsHandler {
    type Event = PullDiagnosticsEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _timeout: Option<tokio::time::Instant>,
    ) -> Option<tokio::time::Instant> {
        self.document_ids.insert(event.document_id);
        Some(Instant::now() + Duration::from_millis(250))
    }

    fn finish_debounce(&mut self) {
        let document_ids = mem::take(&mut self.document_ids);
        job::dispatch_blocking(move |editor, _| {
            for document_id in document_ids {
                request_document_diagnostics(editor, document_id);
            }
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct PullAllDocumentsDiagnosticHandler {
    language_servers: HashSet<LanguageServerId>,
    last_changed_document: Option<DocumentId>,
}

impl helix_event::AsyncHook for PullAllDocumentsDiagnosticHandler {
    type Event = PullAllDocumentsDiagnosticsEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _timeout: Option<tokio::time::Instant>,
    ) -> Option<tokio::time::Instant> {
        // Cancel checks based on the previous dependency state, including checks
        // still waiting for a background slot. The debounce below replaces them.
        let changed_document = event.changed_document;
        let language_servers = event.language_servers.clone();
        if !language_servers.is_empty() {
            job::dispatch_blocking(move |editor, _| {
                let visible: HashSet<_> = editor.tree.views().map(|(view, _)| view.doc).collect();
                for doc in editor.documents.values_mut() {
                    if doc.id() == changed_document {
                        continue;
                    }
                    for server in &language_servers {
                        if let Some(controller) = doc.pull_diagnostic_controllers.get_mut(server) {
                            controller.cancel();
                            // Focus changes must promote dirty buffers even
                            // before the dependency debounce has expired.
                            if !visible.contains(&doc.id()) {
                                doc.background_diagnostic_servers.insert(*server);
                            }
                        }
                    }
                }
            });
        }
        self.language_servers.extend(&event.language_servers);
        // Only the most recently edited document is guaranteed to have a local
        // diagnostic request after every pending dependency change. Earlier
        // edited documents may depend on this one and must still be refreshed.
        self.last_changed_document = Some(event.changed_document);
        Some(Instant::now() + Duration::from_secs(1))
    }

    fn finish_debounce(&mut self) {
        let language_servers = mem::take(&mut self.language_servers);
        let last_changed_document = self.last_changed_document.take();
        job::dispatch_blocking(move |editor, _| {
            let documents: Vec<_> = editor.documents.keys().copied().collect();

            for document in documents {
                if Some(document) == last_changed_document {
                    continue;
                }
                request_document_diagnostics_for_language_servers(
                    editor,
                    document,
                    language_servers.clone(),
                );
            }
        })
    }
}

fn is_visible(editor: &Editor, doc_id: DocumentId) -> bool {
    editor.tree.views().any(|(view, _)| view.doc == doc_id)
}

fn refresh_visible_background_diagnostics(editor: &mut Editor) {
    let visible: HashSet<_> = editor.tree.views().map(|(view, _)| view.doc).collect();
    for doc_id in visible {
        let Some(doc) = editor.document(doc_id) else {
            continue;
        };
        let servers = doc.background_diagnostic_servers.clone();
        if !servers.is_empty() {
            request_document_diagnostics_for_language_servers(editor, doc_id, servers);
        }
    }
}

fn request_document_diagnostics_for_language_servers(
    editor: &mut Editor,
    doc_id: DocumentId,
    language_servers: HashSet<LanguageServerId>,
) {
    let visible = is_visible(editor, doc_id);
    let slots = editor.handlers.background_diagnostic_slots.clone();
    for server_id in language_servers {
        let Some(doc) = editor.document_mut(doc_id) else {
            return;
        };
        if !doc
            .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
            .any(|server| server.id() == server_id)
        {
            continue;
        }
        let cancel = doc
            .pull_diagnostic_controllers
            .entry(server_id)
            .or_default()
            .restart();
        if visible {
            doc.background_diagnostic_servers.remove(&server_id);
            start_diagnostic_request(editor, doc_id, server_id, cancel, None);
        } else {
            doc.background_diagnostic_servers.insert(server_id);
            let slots = slots.clone();
            tokio::spawn(async move {
                let wait = async {
                    // Dependency refresh is already debounced for one second.
                    // Hidden buffers get another second, then at most two checks
                    // can run at once. No LSP request is sent while waiting.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    slots.acquire_owned().await
                };
                let Some(Ok(permit)) = cancelable_future(wait, &cancel).await else {
                    return;
                };
                job::dispatch(move |editor, _| {
                    start_diagnostic_request(editor, doc_id, server_id, cancel, Some(permit));
                })
                .await;
            });
        }
    }
}

fn start_diagnostic_request(
    editor: &mut Editor,
    doc_id: DocumentId,
    server_id: LanguageServerId,
    cancel: TaskHandle,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) {
    if cancel.is_canceled() {
        return;
    }
    let Some(doc) = editor.document(doc_id) else {
        return;
    };
    let Some(server) = doc
        .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
        .find(|server| server.id() == server_id)
    else {
        return;
    };
    let Some(uri) = doc.uri() else {
        return;
    };
    let version = doc.version();
    let identifier = server
        .capabilities()
        .diagnostic_provider
        .as_ref()
        .and_then(|provider| match provider {
            lsp::DiagnosticServerCapabilities::Options(options) => options.identifier.clone(),
            lsp::DiagnosticServerCapabilities::RegistrationOptions(options) => {
                options.diagnostic_options.identifier.clone()
            }
        });
    let provider = DiagnosticProvider::Lsp {
        server_id,
        identifier,
    };
    // Dispatch on the editor thread, after obtaining the slot, to preserve the
    // ordering between didChange notifications and this request's version.
    let Some(future) = server.text_document_diagnostic(
        doc.identifier(),
        doc.previous_diagnostic_ids.get(&server_id).cloned(),
    ) else {
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        let Some(result) = cancelable_future(future, &cancel).await else {
            return;
        };
        match result {
            Ok(result) => {
                job::dispatch(move |editor, _| {
                    if cancel.is_canceled() {
                        return;
                    }
                    let Some(doc) = editor.document_mut(doc_id) else {
                        return;
                    };
                    if doc.version() != version || !doc.supports_language_server(server_id) {
                        return;
                    }
                    doc.background_diagnostic_servers.remove(&server_id);
                    handle_pull_diagnostics_response(editor, result, provider, uri, doc_id);
                })
                .await;
            }
            Err(err) => {
                let retry = match &err {
                    helix_lsp::Error::Rpc(error) => error
                        .data
                        .as_ref()
                        .and_then(|data| {
                            serde_json::from_value::<lsp::DiagnosticServerCancellationData>(
                                data.clone(),
                            )
                            .ok()
                        })
                        .is_some_and(|data| data.retrigger_request),
                    _ => false,
                };
                if !matches!(&err, helix_lsp::Error::Rpc(error) if matches!(error.code.code(), -32802..=-32800))
                {
                    log::error!("Pull diagnostic request failed: {err}");
                }
                if retry
                    && cancelable_future(tokio::time::sleep(Duration::from_millis(500)), &cancel)
                        .await
                        .is_some()
                {
                    job::dispatch(move |editor, _| {
                        if !cancel.is_canceled() {
                            request_document_diagnostics_for_language_servers(
                                editor,
                                doc_id,
                                HashSet::from([server_id]),
                            );
                        }
                    })
                    .await;
                }
            }
        }
    });
}

pub fn request_all_document_diagnostics_for_language_server(
    editor: &mut Editor,
    server_id: LanguageServerId,
) {
    let doc_ids: Vec<_> = editor
        .documents
        .values()
        .filter(|doc| doc.supports_language_server(server_id))
        .map(|doc| doc.id())
        .collect();
    for doc_id in doc_ids {
        request_document_diagnostics_for_language_servers(
            editor,
            doc_id,
            HashSet::from([server_id]),
        );
    }
}

pub fn request_document_diagnostics(editor: &mut Editor, doc_id: DocumentId) {
    let Some(doc) = editor.document(doc_id) else {
        return;
    };

    let language_servers = doc
        .language_servers_with_feature(LanguageServerFeature::PullDiagnostics)
        .map(|language_servers| language_servers.id())
        .collect();

    request_document_diagnostics_for_language_servers(editor, doc_id, language_servers);
}

fn handle_pull_diagnostics_response(
    editor: &mut Editor,
    result: lsp::DocumentDiagnosticReportResult,
    provider: DiagnosticProvider,
    uri: Uri,
    document_id: DocumentId,
) {
    match result {
        lsp::DocumentDiagnosticReportResult::Report(report) => {
            let result_id = match report {
                lsp::DocumentDiagnosticReport::Full(report) => {
                    editor.handle_lsp_diagnostics(
                        &provider,
                        uri,
                        None,
                        report.full_document_diagnostic_report.items,
                    );

                    report.full_document_diagnostic_report.result_id
                }
                lsp::DocumentDiagnosticReport::Unchanged(report) => {
                    Some(report.unchanged_document_diagnostic_report.result_id)
                }
            };

            if let Some(doc) = editor.document_mut(document_id) {
                let server_id = provider
                    .language_server_id()
                    .expect("pull diagnostics always originate from an LSP");
                match result_id {
                    Some(result_id) => {
                        doc.previous_diagnostic_ids.insert(server_id, result_id);
                    }
                    None => {
                        doc.previous_diagnostic_ids.remove(&server_id);
                    }
                }
            };
        }
        lsp::DocumentDiagnosticReportResult::Partial(_) => {}
    };
}
