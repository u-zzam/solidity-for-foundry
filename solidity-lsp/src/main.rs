use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

mod complete;
mod diagnostics;
mod index;
mod parse;
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
    /// Live tree-sitter parse of each open buffer, re-run on every edit. Drives
    /// navigation instantly (no compile, no foundry.toml) and while typing; the
    /// solc index is preferred only when it has valid, non-stale positions.
    parsed: RwLock<HashMap<Url, parse::File>>,
    /// URIs we last published diagnostics for, so we can clear stale ones.
    published: Mutex<HashSet<Url>>,
    /// Serializes project compiles; one solc run at a time is plenty for an editor.
    compiling: Mutex<()>,
    /// Navigation index per Foundry root (keyed by the directory holding
    /// foundry.toml), so several projects open at once — a monorepo with
    /// multiple foundry.toml files — each keep their own index instead of
    /// clobbering a single shared one.
    index: RwLock<HashMap<PathBuf, index::Index>>,
    /// Roots whose full index compile is in flight, so repeated saves of one
    /// project don't pile up while a different project can still index
    /// concurrently (single-flight per root, not globally). A std mutex so the
    /// RAII guard can clear a root on drop (including on panic) without awaiting.
    indexing: std::sync::Mutex<HashSet<PathBuf>>,
    /// Per-root generation counter that debounces index rebuilds: a burst of
    /// saves bumps it repeatedly and only the last one survives the delay, so
    /// the heavy full compile is coalesced and kept off the save's hot path.
    index_gen: Mutex<HashMap<PathBuf, u64>>,
    /// Latest document version per URI, to debounce live as-you-type checks.
    live_versions: Mutex<HashMap<Url, i32>>,
    /// forge lint quick-fixes from the last compile, per URI, for code actions.
    fixes: Mutex<HashMap<Url, Vec<diagnostics::LintFix>>>,
}

/// Clears a root from the in-flight indexing set when dropped, so a panic during
/// the build can't leave the root permanently blocked from re-indexing.
struct IndexingGuard {
    state: Arc<State>,
    root: PathBuf,
}

impl Drop for IndexingGuard {
    fn drop(&mut self) {
        self.state
            .indexing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.root);
    }
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

        let mut new = diagnostics::group(&errors, &root, &trigger);

        // Surface `forge lint` (solar) findings alongside solc diagnostics.
        let r2 = root.clone();
        let lints = tokio::task::spawn_blocking(move || project::lint(&r2))
            .await
            .unwrap_or_default();
        for (uri, mut ds) in diagnostics::group_lints(&lints) {
            new.entry(uri).or_default().append(&mut ds);
        }
        *self.state.fixes.lock().await = diagnostics::lint_fixes(&lints);

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
        // Clear files that had diagnostics last time but are clean now — except
        // files with unsaved edits, whose squiggles are owned by the live buffer
        // check (this on-disk compile would otherwise wipe them until the next
        // keystroke).
        let mut next: HashSet<Url> = new.keys().cloned().collect();
        for uri in published.iter() {
            if new.contains_key(uri) {
                continue;
            }
            if self.is_dirty(uri).await {
                next.insert(uri.clone());
                continue;
            }
            self.client
                .publish_diagnostics(uri.clone(), Vec::new(), None)
                .await;
        }
        *published = next;
    }

    /// The Foundry root whose solc index should answer for `uri`: an index
    /// exists whose stored text for this file is byte-for-byte the live buffer,
    /// so its positions are exactly valid. Returns `None` — "use the live parser
    /// instead" — when the file is unindexed (cold start), has been edited since
    /// the index was built (the parser is then both live and correct), or has no
    /// `foundry.toml` at all.
    async fn valid_index_root(&self, uri: &Url) -> Option<PathBuf> {
        let path = uri.to_file_path().ok()?;
        let root = project::locate_root(&path)?;
        let buffer = self.state.docs.read().await.get(uri).cloned()?;
        let guard = self.state.index.read().await;
        guard.get(&root)?.matches(&path, &buffer).then_some(root)
    }

    /// Whether `uri` has an open buffer that differs from its on-disk content
    /// (so the live check, not an on-disk compile, owns its diagnostics).
    async fn is_dirty(&self, uri: &Url) -> bool {
        let Some(buffer) = self.state.docs.read().await.get(uri).cloned() else {
            return false;
        };
        let Ok(path) = uri.to_file_path() else {
            return false;
        };
        // A buffer with no readable file on disk is unsaved -> live owns it.
        std::fs::read_to_string(&path).map_or(true, |disk| disk != buffer)
    }

    /// Rebuild the accuracy index from a full compile of the owning project.
    /// Single-flighted per root: while one project's build runs, further
    /// requests for the same root are dropped (the next save retriggers), but a
    /// different project can index concurrently. The full compile is the cost of
    /// cross-file node-id consistency — solc only emits a complete, consistent
    /// AST set from a cold compile of every source, so this can't ride the warm
    /// incremental diagnostics compile. The live tree-sitter parser covers
    /// navigation while this refreshes in the background.
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
        {
            let mut inflight = self.state.indexing.lock().unwrap_or_else(|e| e.into_inner());
            if !inflight.insert(root.clone()) {
                return; // this root is already being indexed
            }
        }
        // Clear the in-flight marker on every exit path (including a panic in any
        // of the awaits below), so a failed build never blocks re-indexing.
        let _guard = IndexingGuard { state: self.state.clone(), root: root.clone() };

        // Show "Indexing…" in the editor so navigation-not-ready reads as
        // in-progress, not broken, during the full compile.
        let token = self.progress_begin("Indexing Solidity project").await;
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
        self.progress_end(token).await;
        match built {
            Ok(Ok(Some(idx))) => {
                self.state.index.write().await.insert(root.clone(), idx);
                // Tell the editor to re-pull the features derived from this index.
                // Without these, rebuilt inlay hints and semantic tokens sit unused
                // until the user happens to scroll, edit, or refocus — which reads
                // as hints flickering in and out after a compile. (Both clients
                // advertise refreshSupport, so the re-request actually fires.)
                let _ = self.client.inlay_hint_refresh().await;
                let _ = self.client.semantic_tokens_refresh().await;
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

    /// Begin a work-done progress (shown in the editor's status bar).
    async fn progress_begin(&self, title: &str) -> ProgressToken {
        let token = ProgressToken::String("solidity/indexing".to_string());
        let _ = self
            .client
            .send_request::<request::WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await;
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: title.to_string(),
                        cancellable: Some(false),
                        message: None,
                        percentage: None,
                    },
                )),
            })
            .await;
        token
    }

    async fn progress_end(&self, token: ProgressToken) {
        self.client
            .send_notification::<notification::Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: None,
                })),
            })
            .await;
    }

    /// Run work off the message loop so the server stays responsive.
    fn schedule_diagnostics(&self, uri: Url) {
        let me = self.clone();
        tokio::spawn(async move { me.run_diagnostics(uri).await });
    }

    /// Refresh the accuracy index off the hot path: debounced per root so a run
    /// of saves collapses into a single full compile after editing settles.
    /// Navigation never waits on this — the live parser already answers — so a
    /// short delay only affects when the more precise solc resolution kicks in.
    fn schedule_index(&self, uri: Url) {
        let me = self.clone();
        tokio::spawn(async move {
            let Some(root) =
                uri.to_file_path().ok().and_then(|p| project::locate_root(&p))
            else {
                return;
            };
            let gen = {
                let mut g = me.state.index_gen.lock().await;
                let next = g.get(&root).copied().unwrap_or(0) + 1;
                g.insert(root.clone(), next);
                next
            };
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            // A newer save superseded this one; let that one do the compile.
            if me.state.index_gen.lock().await.get(&root).copied() != Some(gen) {
                return;
            }
            me.build_index(uri).await;
        });
    }

    /// Type-check the buffer immediately (no debounce) — used on open so the
    /// first diagnostics appear in well under a second instead of waiting for
    /// the full codegen compile.
    fn schedule_live_check_now(&self, uri: Url) {
        let me = self.clone();
        tokio::spawn(async move { me.live_check(uri).await });
    }

    /// Debounce, then type-check the live buffer for fast as-you-type feedback.
    fn schedule_live_check(&self, uri: Url, version: i32) {
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            // Skip if a newer edit landed during the debounce window.
            if me.state.live_versions.lock().await.get(&uri).copied() != Some(version) {
                return;
            }
            me.live_check(uri).await;
        });
    }

    /// Type-check the unsaved buffer and publish the edited file's diagnostics.
    /// With a Foundry project this type-checks against the real import graph;
    /// without one it falls back to a standalone single-file check (config-less).
    /// Silently no-ops if no solc version can be determined.
    async fn live_check(&self, uri: Url) {
        let Ok(path) = uri.to_file_path() else {
            return;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("sol") {
            return;
        }
        let Some(buffer) = self.state.docs.read().await.get(&uri).cloned() else {
            return;
        };

        let root = project::locate_root(&path);
        let (r, t, buf) = (root.clone(), path.clone(), buffer.clone());
        let errors = tokio::task::spawn_blocking(move || match r {
            Some(r) => project::check_buffer(&r, &t, &buf),
            None => project::check_standalone(&t, &buf),
        })
        .await;
        let Ok(Ok(errors)) = errors else {
            return;
        };

        // Map positions against the buffer solc actually compiled, not disk. With
        // no project root, errors carry the file's own absolute path, so any base
        // works for the URI match; use the file's parent.
        let base = root.unwrap_or_else(|| path.parent().unwrap_or(&path).to_path_buf());
        let diags = diagnostics::for_buffer(&errors, &base, &uri, &buffer);
        let mut published = self.state.published.lock().await;
        if diags.is_empty() {
            published.remove(&uri);
        } else {
            published.insert(uri.clone());
        }
        self.client.publish_diagnostics(uri, diags, None).await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "solidity-for-foundry-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    ..Default::default()
                }),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                        legend: SemanticTokensLegend {
                            token_types: index::token_legend(),
                            token_modifiers: vec![],
                        },
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                        range: Some(false),
                        work_done_progress_options: Default::default(),
                    }),
                ),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "solidity-for-foundry-lsp ready")
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
        let uri = p.text_document.uri;
        // Accurate solc index, when present and not stale.
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(loc) =
                    guard.get(&root).and_then(|idx| idx.definition(&path, p.position))
                {
                    return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                }
            }
        }
        // Live parser fallback (cold start, mid-edit, or no foundry.toml).
        let parsed = self.state.parsed.read().await;
        let locs = parse::definition(&parsed, &uri, p.position);
        Ok((!locs.is_empty()).then_some(GotoDefinitionResponse::Array(locs)))
    }

    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // The type of a symbol is solc type information — only the index has it.
        let p = params.text_document_position_params;
        let uri = p.text_document.uri;
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(loc) =
                    guard.get(&root).and_then(|idx| idx.type_definition(&path, p.position))
                {
                    return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                }
            }
        }
        Ok(None)
    }

    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // Override resolution needs solc's `baseFunctions` — index only.
        let p = params.text_document_position_params;
        let uri = p.text_document.uri;
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(idx) = guard.get(&root) {
                    let locs = idx.implementations(&path, p.position);
                    if !locs.is_empty() {
                        return Ok(Some(GotoDefinitionResponse::Array(locs)));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let p = params.text_document_position;
        let uri = p.text_document.uri;
        let include = params.context.include_declaration;
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(idx) = guard.get(&root) {
                    let locs = idx.references(&path, p.position, include);
                    if !locs.is_empty() {
                        return Ok(Some(locs));
                    }
                }
            }
        }
        let parsed = self.state.parsed.read().await;
        let locs = parse::references(&parsed, &uri, p.position, include);
        Ok((!locs.is_empty()).then_some(locs))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        // Always live: highlight every occurrence of the name under the cursor in
        // the current buffer, straight from the tree-sitter parse.
        let p = params.text_document_position_params;
        let parsed = self.state.parsed.read().await;
        let Some(file) = parsed.get(&p.text_document.uri) else {
            return Ok(None);
        };
        let hl = parse::highlights(file, p.position);
        Ok((!hl.is_empty()).then_some(hl))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let p = params.text_document_position_params;
        let uri = p.text_document.uri;
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(value) = guard.get(&root).and_then(|idx| idx.hover(&path, p.position)) {
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value,
                        }),
                        range: None,
                    }));
                }
            }
        }
        let parsed = self.state.parsed.read().await;
        Ok(parse::hover(&parsed, &uri, p.position))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(idx) = guard.get(&root) {
                    let syms = idx.document_symbols(&path);
                    if !syms.is_empty() {
                        return Ok(Some(DocumentSymbolResponse::Nested(syms)));
                    }
                }
            }
        }
        let parsed = self.state.parsed.read().await;
        let syms = parsed.get(&uri).map(parse::document_symbols).unwrap_or_default();
        Ok((!syms.is_empty()).then_some(DocumentSymbolResponse::Nested(syms)))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        // Search every open project's index so workspace symbols span a monorepo;
        // fall back to the open buffers before any compile has produced one.
        let guard = self.state.index.read().await;
        if !guard.is_empty() {
            let out: Vec<SymbolInformation> =
                guard.values().flat_map(|idx| idx.workspace_symbols(&params.query)).collect();
            return Ok(Some(out));
        }
        drop(guard);
        let parsed = self.state.parsed.read().await;
        Ok(Some(parse::workspace_symbols(&parsed, &params.query)))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let p = params.text_document_position;
        let uri = p.text_document.uri;
        let Some(text) = self.state.docs.read().await.get(&uri).cloned() else {
            return Ok(None);
        };
        let offset = diagnostics::PositionMapper::new(&text).offset(p.position);

        // Import path: sibling files/dirs relative to this file, plus the
        // project's remapping prefixes. No index or parse needed.
        if let Some(prefix) = complete::import_path_context(&text, offset) {
            let path = uri.to_file_path().ok();
            let dir = path.as_deref().and_then(Path::parent).map(Path::to_path_buf);
            let remaps = path
                .as_deref()
                .and_then(project::locate_root)
                .map(|r| project::remapping_prefixes(&r))
                .unwrap_or_default();
            let items =
                dir.map(|d| complete::import_completions(&d, &prefix, &remaps)).unwrap_or_default();
            return Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)));
        }

        let container = member_context(&text, offset);
        let mut items = Vec::new();
        match &container {
            Some(c) => {
                // Magic-global members (msg./block./tx./abi.) are always available.
                items.extend(complete::member_builtins(c));
                // User-defined members: accurate index when valid, else the parser.
                if let Some(root) = self.valid_index_root(&uri).await {
                    if let Some(idx) = self.state.index.read().await.get(&root) {
                        items.extend(idx.member_completions(c));
                    }
                } else {
                    let parsed = self.state.parsed.read().await;
                    items.extend(parse::member_completions(&parsed, c));
                }
            }
            None => {
                // User-defined symbols in scope (index when valid, else parser).
                if let Some(root) = self.valid_index_root(&uri).await {
                    if let Some(idx) = self.state.index.read().await.get(&root) {
                        items.extend(idx.global_completions());
                    }
                } else {
                    let parsed = self.state.parsed.read().await;
                    items.extend(parse::global_completions(&parsed));
                }
                // Keywords, global builtins and snippets — offered before any
                // index or parse has project-specific symbols.
                items.extend(complete::keywords());
                items.extend(complete::global_builtins());
                items.extend(complete::snippets());
            }
        }
        Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let p = params.text_document_position_params;
        let Some(text) = self.state.docs.read().await.get(&p.text_document.uri).cloned() else {
            return Ok(None);
        };
        let Some(root) = p.text_document.uri.to_file_path().ok().and_then(|path| project::locate_root(&path)) else {
            return Ok(None);
        };
        let offset = diagnostics::PositionMapper::new(&text).offset(p.position);
        let Some((callee, active)) = call_context(&text, offset) else {
            return Ok(None);
        };
        let guard = self.state.index.read().await;
        let Some(idx) = guard.get(&root) else {
            return Ok(None);
        };
        Ok(idx.signatures(&callee, active))
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let req = params.range;
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        // `forge lint` suggested replacements (from the last compile).
        if let Some(file_fixes) = self.state.fixes.lock().await.get(&uri) {
            for f in file_fixes.iter().filter(|f| ranges_overlap(f.range, req)) {
                actions.push(quickfix(&uri, f.range, f.new_text.clone(), f.title.clone()));
            }
        }

        // The rest need the current buffer text.
        let text = self.state.docs.read().await.get(&uri).cloned();
        if let Some(text) = text {
            // Missing SPDX is suppressed in diagnostics to match `forge build`,
            // so offer the fix from the buffer when editing near the top.
            if req.start.line <= 2 && !text.contains("SPDX-License-Identifier") {
                let (range, new_text) = header_edit(&text, 0, "// SPDX-License-Identifier: MIT");
                actions.push(quickfix(&uri, range, new_text, "Add SPDX license identifier".into()));
            }

            for d in &params.context.diagnostics {
                // Missing pragma: solc names the exact pragma to add.
                if diag_code_is(d, 3420) {
                    if let Some(pragma) = pragma_from_message(&d.message) {
                        let line = spdx_line(&text).map_or(0, |l| l + 1);
                        let (range, new_text) = header_edit(&text, line, &pragma);
                        actions.push(quickfix(&uri, range, new_text, format!("Add `{pragma}`")));
                    }
                }
                // Undeclared identifier: suggest importing it from where it lives.
                if diag_code_is(d, 7576) {
                    let name = slice(&text, d.range);
                    if !name.is_empty() && name.bytes().all(is_ident_byte) {
                        let from = uri.to_file_path().ok();
                        let root = from.as_deref().and_then(project::locate_root);
                        let guard = self.state.index.read().await;
                        let idx = root.as_ref().and_then(|r| guard.get(r));
                        if let (Some(idx), Some(from)) = (idx, from) {
                            for cand in idx.import_candidates(&name) {
                                if cand == from {
                                    continue; // never import a file into itself
                                }
                                let Some(rel) = relative_import(&from, &cand) else {
                                    continue;
                                };
                                let stmt = format!("import {{{name}}} from \"{rel}\";");
                                let (range, new_text) =
                                    header_edit(&text, import_line(&text), &stmt);
                                actions.push(quickfix(
                                    &uri,
                                    range,
                                    new_text,
                                    format!("Import `{name}` from \"{rel}\""),
                                ));
                            }
                        }
                    }
                }
            }
        }

        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return Ok(None);
        };
        let Some(root) = project::locate_root(&path) else {
            return Ok(None);
        };
        let guard = self.state.index.read().await;
        let Some(idx) = guard.get(&root) else {
            return Ok(None);
        };
        let data = idx.semantic_tokens(&path);
        Ok((!data.is_empty())
            .then_some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data })))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let Ok(path) = params.text_document.uri.to_file_path() else {
            return Ok(None);
        };
        let Some(root) = project::locate_root(&path) else {
            return Ok(None);
        };
        let guard = self.state.index.read().await;
        let Some(idx) = guard.get(&root) else {
            return Ok(None);
        };
        let hints = idx.inlay_hints(&path, params.range);
        Ok((!hints.is_empty()).then_some(hints))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        // Offer rename only where the accurate index can actually carry it out,
        // so the editor's pre-flight matches what `rename` will do.
        let uri = params.text_document.uri;
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(range) =
                    guard.get(&root).and_then(|idx| idx.rename_range(&path, params.position))
                {
                    return Ok(Some(PrepareRenameResponse::Range(range)));
                }
            }
        }
        Ok(None)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        // Reject names solc would — empty, bad characters, or reserved words —
        // before touching the document, surfacing the reason to the editor.
        if let Err(msg) = valid_new_name(&params.new_name) {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(msg));
        }
        let p = params.text_document_position;
        let uri = p.text_document.uri;
        // Rename through the accurate index only (name-based parser rename could
        // hit unrelated same-named symbols); needs valid, non-stale positions.
        let Some(root) = self.valid_index_root(&uri).await else {
            return Ok(None);
        };
        let Ok(path) = uri.to_file_path() else {
            return Ok(None);
        };
        let guard = self.state.index.read().await;
        let Some(idx) = guard.get(&root) else {
            return Ok(None);
        };
        Ok(idx.rename(&path, p.position, &params.new_name))
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
        // Parse immediately so navigation works the instant the file opens —
        // before (and independent of) the background compile and index.
        let file = parse::parse(&doc.text);
        self.state.docs.write().await.insert(doc.uri.clone(), doc.text);
        self.state.parsed.write().await.insert(doc.uri.clone(), file);
        // Fast type-check first (instant feedback), then the full codegen
        // compile + navigation index in the background.
        self.schedule_live_check_now(doc.uri.clone());
        self.schedule_diagnostics(doc.uri.clone());
        self.schedule_index(doc.uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        // FULL sync: the last change carries the entire document text. Re-parse
        // just this buffer so navigation stays live while typing.
        if let Some(change) = params.content_changes.into_iter().next_back() {
            let file = parse::parse(&change.text);
            self.state.docs.write().await.insert(uri.clone(), change.text);
            self.state.parsed.write().await.insert(uri.clone(), file);
        }
        self.state.live_versions.lock().await.insert(uri.clone(), version);
        self.schedule_live_check(uri, version);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.schedule_diagnostics(params.text_document.uri.clone());
        self.schedule_index(params.text_document.uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.state.docs.write().await.remove(&params.text_document.uri);
        self.state.parsed.write().await.remove(&params.text_document.uri);
    }
}

/// Whether two LSP ranges intersect (touching counts), so a code action is
/// offered when the cursor/selection meets a fix's span.
fn ranges_overlap(a: Range, b: Range) -> bool {
    a.start <= b.end && b.start <= a.end
}

/// Build a single-edit quick-fix code action.
fn quickfix(uri: &Url, range: Range, new_text: String, title: String) -> CodeActionOrCommand {
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
    CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
        ..Default::default()
    })
}

/// Whether a diagnostic carries the given numeric solc error code.
fn diag_code_is(d: &Diagnostic, code: i32) -> bool {
    matches!(&d.code, Some(NumberOrString::Number(n)) if *n == code)
}

/// Pull the suggested pragma out of solc's "does not specify required compiler
/// version" message, which names it verbatim in quotes.
fn pragma_from_message(msg: &str) -> Option<String> {
    let start = msg.find('"')? + 1;
    let end = msg[start..].find('"')? + start;
    let candidate = msg[start..end].trim();
    candidate.starts_with("pragma").then(|| candidate.to_string())
}

/// 0-based line of the SPDX identifier, if the file has one.
fn spdx_line(text: &str) -> Option<u32> {
    text.lines().position(|l| l.contains("SPDX-License-Identifier")).map(|i| i as u32)
}

/// Line to insert a new import on: after the last import/pragma, else after the
/// SPDX line, else the top of the file.
fn import_line(text: &str) -> u32 {
    let mut after = spdx_line(text).map_or(0, |l| l + 1);
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("import") || t.starts_with("pragma") {
            after = i as u32 + 1;
        }
    }
    after
}

/// A text edit inserting `stmt` (without a trailing newline) as its own line at
/// 0-based `line`. If that line is past the end of a file with no trailing
/// newline, append after the last line with a leading newline so the statement
/// isn't jammed onto existing code.
fn header_edit(text: &str, line: u32, stmt: &str) -> (Range, String) {
    let newlines = text.matches('\n').count() as u32;
    if line <= newlines {
        let at = Position::new(line, 0);
        (Range::new(at, at), format!("{stmt}\n"))
    } else {
        let end = diagnostics::PositionMapper::new(text).position(text.len());
        (Range::new(end, end), format!("\n{stmt}"))
    }
}

/// The document text covered by an LSP range.
fn slice(text: &str, range: Range) -> String {
    let m = diagnostics::PositionMapper::new(text);
    let (s, e) = (m.offset(range.start), m.offset(range.end));
    text.get(s..e).unwrap_or("").to_string()
}

/// A relative Solidity import path from one file to another (`./B.sol`,
/// `../lib/C.sol`), which resolves the same regardless of remappings.
fn relative_import(from_file: &std::path::Path, to_file: &std::path::Path) -> Option<String> {
    let from: Vec<_> = from_file.parent()?.components().collect();
    let to: Vec<_> = to_file.components().collect();
    let mut i = 0;
    while i < from.len() && i < to.len() && from[i] == to[i] {
        i += 1;
    }
    let mut s = String::new();
    match from.len() - i {
        0 => s.push_str("./"),
        ups => (0..ups).for_each(|_| s.push_str("../")),
    }
    let rest: Vec<String> =
        to[i..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if rest.is_empty() {
        return None;
    }
    s.push_str(&rest.join("/"));
    Some(s)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Validate a rename target is a legal Solidity identifier and not reserved.
fn valid_new_name(name: &str) -> std::result::Result<(), String> {
    match name.bytes().next() {
        None => return Err("name cannot be empty".to_string()),
        Some(b) if b.is_ascii_digit() => {
            return Err("identifier cannot start with a digit".to_string())
        }
        Some(b) if !is_ident_byte(b) => {
            return Err("identifier must start with a letter, '_' or '$'".to_string())
        }
        Some(_) => {}
    }
    if !name.bytes().all(is_ident_byte) {
        return Err("identifier may only contain letters, digits, '_' or '$'".to_string());
    }
    if complete::is_reserved(name) {
        return Err(format!("`{name}` is a reserved word"));
    }
    Ok(())
}

/// If the cursor is completing `<ident>.<partial>`, return `<ident>`.
fn member_context(text: &str, offset: usize) -> Option<String> {
    let b = text.as_bytes();
    let mut i = offset.min(b.len());
    while i > 0 && is_ident_byte(b[i - 1]) {
        i -= 1; // skip the partial member being typed
    }
    if i == 0 || b[i - 1] != b'.' {
        return None;
    }
    let mut k = i - 1; // at the '.'
    while k > 0 && b[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    let end = k;
    while k > 0 && is_ident_byte(b[k - 1]) {
        k -= 1;
    }
    (k < end).then(|| text[k..end].to_string())
}

/// Find the enclosing call for signature help: `(callee_name, active_param)`.
/// A backward paren scan; it does not skip strings/comments (good enough live).
fn call_context(text: &str, offset: usize) -> Option<(String, u32)> {
    let b = text.as_bytes();
    let mut i = offset.min(b.len());
    let mut depth = 0i32;
    let mut commas = 0u32;
    while i > 0 {
        i -= 1;
        match b[i] {
            b')' => depth += 1,
            b'(' if depth > 0 => depth -= 1,
            b'(' => {
                let mut k = i;
                while k > 0 && b[k - 1].is_ascii_whitespace() {
                    k -= 1;
                }
                let end = k;
                while k > 0 && is_ident_byte(b[k - 1]) {
                    k -= 1;
                }
                return (k < end).then(|| (text[k..end].to_string(), commas));
            }
            b',' if depth == 0 => commas += 1,
            b';' | b'{' | b'}' if depth == 0 => return None,
            _ => {}
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::{
        call_context, header_edit, import_line, member_context, pragma_from_message,
        relative_import, valid_new_name,
    };
    use std::path::Path;
    use tower_lsp::lsp_types::Position;

    #[test]
    fn rename_target_validation() {
        assert!(valid_new_name("totalSupply").is_ok());
        assert!(valid_new_name("_x").is_ok());
        assert!(valid_new_name("$y2").is_ok());
        // Empty, digit-led, bad characters and reserved words are rejected.
        assert!(valid_new_name("").is_err());
        assert!(valid_new_name("2cool").is_err());
        assert!(valid_new_name("a-b").is_err());
        assert!(valid_new_name("contract").is_err());
        assert!(valid_new_name("uint256").is_err());
    }

    #[test]
    fn header_edit_handles_missing_trailing_newline() {
        // Normal: insert as its own line.
        let (r, t) = header_edit("a\nb\n", 1, "X");
        assert_eq!(r.start, Position::new(1, 0));
        assert_eq!(t, "X\n");
        // Past EOF with no trailing newline: append after the last line.
        let (r, t) = header_edit("pragma solidity 0.8.35;", 1, "import {Foo} from \"./F.sol\";");
        assert_eq!(r.start, Position::new(0, 23));
        assert_eq!(t, "\nimport {Foo} from \"./F.sol\";");
    }

    #[test]
    fn pragma_extracted_from_solc_message() {
        let msg = "Source file does not specify required compiler version! \
                   Consider adding \"pragma solidity ^0.8.35;\"";
        assert_eq!(pragma_from_message(msg), Some("pragma solidity ^0.8.35;".into()));
        assert_eq!(pragma_from_message("no suggestion here"), None);
    }

    #[test]
    fn relative_import_paths() {
        let a = Path::new("/p/src/A.sol");
        assert_eq!(relative_import(a, Path::new("/p/src/B.sol")), Some("./B.sol".into()));
        assert_eq!(relative_import(a, Path::new("/p/src/lib/C.sol")), Some("./lib/C.sol".into()));
        assert_eq!(
            relative_import(Path::new("/p/src/sub/A.sol"), Path::new("/p/src/B.sol")),
            Some("../B.sol".into())
        );
    }

    #[test]
    fn import_inserts_after_header() {
        assert_eq!(import_line("// SPDX-License-Identifier: MIT\npragma solidity 0.8.35;\n\nx"), 2);
        assert_eq!(import_line("pragma solidity 0.8.35;\nimport \"a.sol\";\ncode"), 2);
        assert_eq!(import_line("contract C {}"), 0);
    }

    #[test]
    fn member_contexts() {
        assert_eq!(member_context("MathLib.mi", 10), Some("MathLib".into()));
        assert_eq!(member_context("MathLib.", 8), Some("MathLib".into()));
        assert_eq!(member_context("foo bar", 7), None);
    }

    #[test]
    fn call_contexts() {
        assert_eq!(call_context("min(a, b", 8), Some(("min".into(), 1)));
        assert_eq!(call_context("min(", 4), Some(("min".into(), 0)));
        // nested call resolves to the inner callee and its active param
        assert_eq!(call_context("min(a, max(b,", 13), Some(("max".into(), 1)));
        // a statement boundary means we are not inside a call
        assert_eq!(call_context("x = 1; y", 8), None);
    }
}
