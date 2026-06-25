use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

mod diagnostics;
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
        let errors = match tokio::task::spawn_blocking(move || project::compile(&r)).await {
            Ok(Ok(errors)) => errors,
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

    /// Run diagnostics off the message loop so the server stays responsive.
    fn schedule_diagnostics(&self, uri: Url) {
        let me = self.clone();
        tokio::spawn(async move { me.run_diagnostics(uri).await });
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
        self.schedule_diagnostics(doc.uri);
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
        self.schedule_diagnostics(params.text_document.uri);
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
