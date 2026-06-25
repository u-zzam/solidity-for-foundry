use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

mod diagnostics;
mod index;
mod project;

#[derive(Clone)]
struct Backend {
    client: Client,
    state: Arc<State>,
}

#[derive(Default)]
struct State {
    /// Open document buffers, keyed by URI. Full-text sync keeps these current.
    docs: RwLock<HashMap<Url, String>>,
    /// URIs we last published diagnostics for, so we can clear stale ones.
    published: Mutex<HashSet<Url>>,
    /// Serializes project compiles; one solc run at a time is plenty for an editor.
    compiling: Mutex<()>,
    /// Navigation index from the last full compile.
    index: RwLock<Option<index::Index>>,
    /// Single-flights the full index compile so saves don't pile up.
    index_lock: Mutex<()>,
}

impl Backend {
    /// Compile the Foundry project owning `trigger` and publish its diagnostics.
    async fn run_diagnostics(&self, trigger: Url) {
        let Ok(path) = trigger.to_file_path() else {
            return;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("sol") {
            return;
        }
        let Some(root) = project::locate_root(&path) else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("no foundry.toml found above {}", path.display()),
                )
                .await;
            return;
        };

        let _guard = self.state.compiling.lock().await;
        let r = root.clone();
        let errors = match tokio::task::spawn_blocking(move || project::compile(&r, false)).await {
            Ok(Ok(out)) => out.errors,
            Ok(Err(e)) => {
                self.client
                    .log_message(MessageType::ERROR, format!("compile failed: {e}"))
                    .await;
                return;
            }
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("compile task failed: {e}"))
                    .await;
                return;
            }
        };

        let new = diagnostics::group(&errors, &root, &trigger);
        let total: usize = new.values().map(Vec::len).sum();
        self.client
            .log_message(
                MessageType::INFO,
                format!("compiled {}: {total} diagnostics across {} files", root.display(), new.len()),
            )
            .await;

        let mut published = self.state.published.lock().await;
        for (uri, diags) in &new {
            self.client
                .publish_diagnostics(uri.clone(), diags.clone(), None)
                .await;
        }
        // Clear files that had diagnostics last time but are clean now.
        for uri in published.iter() {
            if !new.contains_key(uri) {
                self.client
                    .publish_diagnostics(uri.clone(), Vec::new(), None)
                    .await;
            }
        }
        *published = new.into_keys().collect();
    }

    /// Rebuild the navigation index from a full compile of the owning project.
    /// Single-flighted: while one build runs, further requests are dropped (the
    /// next save retriggers). The full compile is the cost of cross-file node-id
    /// consistency; solar will make this live in a later phase.
    async fn build_index(&self, uri: Url) {
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("sol") {
            return;
        }
        let Some(root) = project::locate_root(&path) else {
            return;
        };
        let Ok(_guard) = self.state.index_lock.try_lock() else {
            return;
        };

        let r = root.clone();
        let built = tokio::task::spawn_blocking(move || {
            project::compile(&r, true).map(|out| {
                // solc emits no ASTs when the project has errors. Keep the last
                // good index in that case so navigation stays usable (stale)
                // while broken code is being edited.
                (!out.sources.is_empty()).then(|| index::Index::build(&out.sources))
            })
        })
        .await;
        match built {
            Ok(Ok(Some(idx))) => {
                *self.state.index.write().await = Some(idx);
                self.client
                    .log_message(MessageType::INFO, format!("indexed {}", root.display()))
                    .await;
            }
            Ok(Ok(None)) => {
                self.client
                    .log_message(
                        MessageType::INFO,
                        "index unchanged (project has compile errors)".to_string(),
                    )
                    .await;
            }
            Ok(Err(e)) => {
                self.client
                    .log_message(MessageType::ERROR, format!("index build failed: {e}"))
                    .await;
            }
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("index task failed: {e}"))
                    .await;
            }
        }
    }

    /// Run work off the message loop so the server stays responsive.
    fn schedule_diagnostics(&self, uri: Url) {
        let me = self.clone();
        tokio::spawn(async move { me.run_diagnostics(uri).await });
    }

    fn schedule_index(&self, uri: Url) {
        let me = self.clone();
        tokio::spawn(async move { me.build_index(uri).await });
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "solidity-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "solidity-lsp ready")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let p = params.text_document_position_params;
        let Ok(path) = p.text_document.uri.to_file_path() else {
            return Ok(None);
        };
        let guard = self.state.index.read().await;
        let Some(idx) = guard.as_ref() else {
            return Ok(None);
        };
        Ok(idx
            .definition(&path, p.position)
            .map(GotoDefinitionResponse::Scalar))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let p = params.text_document_position;
        let Ok(path) = p.text_document.uri.to_file_path() else {
            return Ok(None);
        };
        let guard = self.state.index.read().await;
        let Some(idx) = guard.as_ref() else {
            return Ok(None);
        };
        let locs = idx.references(&path, p.position, params.context.include_declaration);
        Ok((!locs.is_empty()).then_some(locs))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let p = params.text_document_position_params;
        let Ok(path) = p.text_document.uri.to_file_path() else {
            return Ok(None);
        };
        let guard = self.state.index.read().await;
        let Some(idx) = guard.as_ref() else {
            return Ok(None);
        };
        Ok(idx.hover(&path, p.position).map(|value| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return Ok(None);
        };
        let guard = self.state.index.read().await;
        let Some(idx) = guard.as_ref() else {
            return Ok(None);
        };
        let syms = idx.document_symbols(&path);
        Ok((!syms.is_empty()).then_some(DocumentSymbolResponse::Nested(syms)))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let guard = self.state.index.read().await;
        let Some(idx) = guard.as_ref() else {
            return Ok(None);
        };
        Ok(Some(idx.workspace_symbols(&params.query)))
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let Some(text) = self.state.docs.read().await.get(&uri).cloned() else {
            return Ok(None);
        };
        let root = uri.to_file_path().ok().and_then(|p| project::locate_root(&p));
        let src = text.clone();
        let formatted =
            tokio::task::spawn_blocking(move || project::format(root.as_deref(), &src))
                .await
                .ok()
                .flatten();
        Ok(formatted.map(|new_text| {
            vec![TextEdit {
                range: diagnostics::full_range(&text),
                new_text,
            }]
        }))
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        self.state.docs.write().await.insert(doc.uri.clone(), doc.text);
        self.schedule_diagnostics(doc.uri.clone());
        self.schedule_index(doc.uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // FULL sync: the last change carries the entire document text.
        if let Some(change) = params.content_changes.into_iter().next_back() {
            self.state
                .docs
                .write()
                .await
                .insert(params.text_document.uri, change.text);
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.schedule_diagnostics(params.text_document.uri.clone());
        self.schedule_index(params.text_document.uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.state.docs.write().await.remove(&params.text_document.uri);
    }
}

#[tokio::main]
async fn main() {
    let (service, socket) = LspService::new(|client| Backend {
        client,
        state: Arc::new(State::default()),
    });
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
