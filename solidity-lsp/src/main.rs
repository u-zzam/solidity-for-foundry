use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// The server-side command a "▶ Run test" code lens invokes. The client library
/// forwards it to `execute_command` because it's in `executeCommandProvider`, so
/// no client-side handler is needed.
const RUN_TEST_COMMAND: &str = "solidity.runTest";

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
    /// Per Foundry root, a background tree-sitter parse of every project source
    /// (src/test/script). It lets go-to-definition, references, workspace symbols
    /// and member completion span the whole project the moment a root is
    /// recognized — before any successful compile and through broken builds, when
    /// only open buffers would otherwise resolve. Open buffers are mirrored in
    /// live so their unsaved content wins over the disk snapshot.
    workspace_parsed: RwLock<HashMap<PathBuf, HashMap<Url, parse::File>>>,
    /// Roots whose background parse has been kicked off, so the one-shot glob +
    /// parse runs once per root (the watched-files handler re-arms it on external
    /// change).
    workspace_roots: Mutex<HashSet<PathBuf>>,
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
    /// Per-root generation counter that debounces diagnostics compiles the same
    /// way `index_gen` debounces index rebuilds. Restoring a session opens N tabs
    /// of one project at once; without this each queued a whole-project compile
    /// plus a full `forge lint` subprocess (which, unlike solc, has no cache).
    diag_gen: Mutex<HashMap<PathBuf, u64>>,
    /// Latest document version per URI, to debounce live as-you-type checks.
    live_versions: Mutex<HashMap<Url, i32>>,
    /// Source-tree signature of each root's last successful index build, so a
    /// rebuild whose sources are byte-for-byte unchanged (e.g. a save with no
    /// edits, or a watched-files event on an untouched file) can skip the full
    /// cold compile instead of reproducing the identical AST.
    index_fingerprint: Mutex<HashMap<PathBuf, u64>>,
    /// forge lint quick-fixes from the last compile, per URI, for code actions.
    fixes: Mutex<HashMap<Url, Vec<diagnostics::LintFix>>>,
    /// Roots we've already warned about an unparseable foundry.toml, so the
    /// "using default settings" notice logs once, not on every compile.
    warned_config: Mutex<HashSet<PathBuf>>,
    /// Roots we've already shown a live-check (solc install/compile) failure for,
    /// so a first-run failure is surfaced once instead of on every keystroke.
    warned_solc: Mutex<HashSet<PathBuf>>,
    /// Whether the client supports dynamic registration of file watchers. VS
    /// Code's client library also delivers watched-file events via its own
    /// `synchronize.fileEvents` convenience, but Zed only sends them for
    /// watchers the server registers over the protocol — so `initialized`
    /// registers one, gated on this capability read from `initialize`.
    watch_dynamic_registration: AtomicBool,
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

        // A foundry.toml that won't parse silently falls back to default
        // settings (no pinned solc, no inline remappings), so diagnostics can
        // drift from `forge build`. Surface that once per root.
        if let Some(err) = project::config_parse_error(&root) {
            if self.state.warned_config.lock().await.insert(root.clone()) {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!(
                            "foundry.toml in {} failed to parse ({err}); using default settings",
                            root.display()
                        ),
                    )
                    .await;
            }
        }

        let _guard = self.state.compiling.lock().await;
        let r = root.clone();
        let (errors, compiled) = match tokio::task::spawn_blocking(move || project::compile(&r, false)).await {
            Ok(Ok(out)) => (out.errors, out.compiled),
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
        // Replace only this root's lint fixes. `fixes` is one global map shared
        // across every open project, so overwriting it wholesale would drop
        // another root's quick-fixes; keep entries outside this root, swap ours.
        {
            let mut fixes = self.state.fixes.lock().await;
            fixes.retain(|uri, _| uri.to_file_path().map_or(true, |p| !p.starts_with(&root)));
            fixes.extend(diagnostics::lint_fixes(&lints));
        }

        let total: usize = new.values().map(Vec::len).sum();
        self.client
            .log_message(
                MessageType::INFO,
                format!("compiled {}: {total} diagnostics across {} files", root.display(), new.len()),
            )
            .await;

        let mut published = self.state.published.lock().await;
        // Files this on-disk compile reported on. Dirty buffers stay seeded here
        // but are not republished below — the live buffer check owns them.
        let mut next: HashSet<Url> = new.keys().cloned().collect();
        for (uri, diags) in &new {
            // Skip files with unsaved edits: their squiggles come from the live
            // buffer check against the in-memory text. Publishing disk-mapped
            // ranges here would jump them to stale offsets (and briefly resurrect
            // a just-fixed error) until the next keystroke.
            if self.is_dirty(uri).await {
                continue;
            }
            self.client
                .publish_diagnostics(uri.clone(), diags.clone(), None)
                .await;
        }
        // Clear files that had diagnostics last time but are clean now — except
        // files with unsaved edits, whose squiggles are owned by the live buffer
        // check (this on-disk compile would otherwise wipe them until the next
        // keystroke).
        for uri in published.iter() {
            if new.contains_key(uri) {
                continue;
            }
            // Only clear a file this compile actually re-checked. A warm-cache
            // hit isn't in `compiled`, so its still-valid warnings survive
            // (foundry's cache doesn't persist diagnostics, so a cache-hit file
            // re-emits none); another root's or a standalone file's diagnostics
            // are likewise left intact. A genuine fix always recompiles the
            // edited file, so real fixes still clear.
            let recompiled = uri.to_file_path().is_ok_and(|p| compiled.contains(&p));
            if !recompiled || self.is_dirty(uri).await {
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

    /// Run a live-parser navigation query for `uri` against the best map: the
    /// owning root's whole-project background parse (open buffers overlaid) when
    /// it's ready, else just the open buffers. Consulted only when the solc index
    /// can't answer (cold start, broken build, config-less file), so a query
    /// still spans the project instead of only what happens to be open.
    async fn with_nav_map<T>(
        &self,
        uri: &Url,
        f: impl FnOnce(&HashMap<Url, parse::File>) -> T,
    ) -> T {
        if let Some(root) = uri.to_file_path().ok().and_then(|p| project::locate_root(&p)) {
            let ws = self.state.workspace_parsed.read().await;
            if let Some(map) = ws.get(&root) {
                return f(map);
            }
        }
        let parsed = self.state.parsed.read().await;
        f(&parsed)
    }

    /// Parse every project source of `root` in the background (once per root), so
    /// navigation spans the whole project even before the first compile. Off the
    /// message loop; the live parser answers meanwhile. Any currently-open buffer
    /// is overlaid so its unsaved parse wins over the disk snapshot.
    fn schedule_workspace_parse(&self, root: PathBuf) {
        let me = self.clone();
        tokio::spawn(async move {
            if !me.state.workspace_roots.lock().await.insert(root.clone()) {
                return; // already parsed (re-armed by the watched-files handler)
            }
            let r = root.clone();
            let disk = tokio::task::spawn_blocking(move || {
                project::source_files(&r)
                    .into_iter()
                    .filter_map(|path| {
                        let text = std::fs::read_to_string(&path).ok()?;
                        Some((Url::from_file_path(&path).ok()?, parse::parse(&text)))
                    })
                    .collect::<HashMap<Url, parse::File>>()
            })
            .await;
            let Ok(mut map) = disk else {
                me.state.workspace_roots.lock().await.remove(&root);
                return;
            };
            // Overlay open buffers in this root: their unsaved text supersedes the
            // disk snapshot read above.
            {
                let parsed = me.state.parsed.read().await;
                for (uri, file) in parsed.iter() {
                    if uri.to_file_path().ok().and_then(|p| project::locate_root(&p)).as_deref()
                        == Some(root.as_path())
                    {
                        map.insert(uri.clone(), file.clone());
                    }
                }
            }
            me.state.workspace_parsed.write().await.insert(root, map);
        });
    }

    /// Mirror an open buffer's fresh parse into its root's whole-project map, so
    /// project-wide navigation sees unsaved edits (open buffers win over the disk
    /// snapshot). A no-op until the root's background parse has populated the map.
    async fn sync_workspace_buffer(&self, uri: &Url, file: parse::File) {
        let Some(root) = uri.to_file_path().ok().and_then(|p| project::locate_root(&p)) else {
            return;
        };
        if let Some(map) = self.state.workspace_parsed.write().await.get_mut(&root) {
            map.insert(uri.clone(), file);
        }
    }

    /// Restore the on-disk parse of `uri` in its root's whole-project map after a
    /// buffer with unsaved edits is closed (the edits are discarded, so the map's
    /// mirrored copy is now stale). Drops the entry if the file is gone from disk.
    fn refresh_workspace_file(&self, uri: Url) {
        let me = self.clone();
        tokio::spawn(async move {
            let Some(root) = uri.to_file_path().ok().and_then(|p| project::locate_root(&p)) else {
                return;
            };
            // Only touch a root that's already been parsed in the background.
            if !me.state.workspace_parsed.read().await.contains_key(&root) {
                return;
            }
            let u = uri.clone();
            let parsed = tokio::task::spawn_blocking(move || {
                u.to_file_path()
                    .ok()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .map(|t| parse::parse(&t))
            })
            .await
            .ok()
            .flatten();
            if let Some(map) = me.state.workspace_parsed.write().await.get_mut(&root) {
                match parsed {
                    Some(file) => {
                        map.insert(uri, file);
                    }
                    None => {
                        map.remove(&uri);
                    }
                }
            }
        });
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

        // Skip a rebuild that would reproduce the current index: if no source file
        // changed since the last successful build, the full cold compile is wasted
        // work (the live parser already answers navigation regardless).
        let fingerprint = {
            let r = root.clone();
            tokio::task::spawn_blocking(move || project::source_fingerprint(&r)).await.ok()
        };
        if let Some(fp) = fingerprint {
            let unchanged = self.state.index_fingerprint.lock().await.get(&root) == Some(&fp)
                && self.state.index.read().await.contains_key(&root);
            if unchanged {
                return;
            }
        }

        // Show "Indexing…" in the editor so navigation-not-ready reads as
        // in-progress, not broken, during the full compile.
        let token = self.progress_begin("Indexing Solidity project").await;
        let r = root.clone();
        let built = {
            // Share the diagnostics compile lock so the index's full solc run and a
            // diagnostics compile never drive two solc pipelines at once.
            let _compiling = self.state.compiling.lock().await;
            tokio::task::spawn_blocking(move || {
                project::compile(&r, true).map(|out| {
                    // solc emits no ASTs when the project has errors. Keep the last
                    // good index in that case so navigation stays usable (stale)
                    // while broken code is being edited.
                    (!out.sources.is_empty()).then(|| index::Index::build(&out.sources))
                })
            })
            .await
        };
        self.progress_end(token).await;
        match built {
            Ok(Ok(Some(idx))) => {
                self.state.index.write().await.insert(root.clone(), idx);
                if let Some(fp) = fingerprint {
                    self.state.index_fingerprint.lock().await.insert(root.clone(), fp);
                }
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

    /// Begin a work-done progress (shown in the editor's status bar). Each call
    /// mints a unique token: indexing runs single-flight per root but several
    /// roots index concurrently, and reusing one token would make the second
    /// `Create` a protocol violation and let the first `End` dismiss the other
    /// root's still-running spinner.
    async fn progress_begin(&self, title: &str) -> ProgressToken {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let token = ProgressToken::String(format!("solidity/indexing/{n}"));
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

    /// Dynamically register a watcher for the project files whose changes affect
    /// compilation — `.sol` sources plus `foundry.toml` / `remappings.txt` — so
    /// `did_change_watched_files` fires in every client, not only ones that ship
    /// their own convenience watcher.
    async fn register_file_watchers(&self) {
        let options = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![FileSystemWatcher {
                glob_pattern: GlobPattern::String(
                    "**/{*.sol,foundry.toml,remappings.txt}".to_string(),
                ),
                kind: None,
            }],
        };
        let registration = Registration {
            id: "solidity-watched-files".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: serde_json::to_value(options).ok(),
        };
        if let Err(e) = self.client.register_capability(vec![registration]).await {
            self.client
                .log_message(MessageType::WARNING, format!("could not register file watcher: {e}"))
                .await;
        }
    }

    /// Run a single Foundry test (`forge test --match-contract C --match-test T
    /// -vvv`) off the message loop and report it: a status-bar spinner while it
    /// runs, the full traces to the log channel, and a one-line pass/fail popup.
    async fn run_forge_test(&self, root: PathBuf, contract: String, test: String) {
        let token = self.progress_begin(&format!("Running {contract}.{test}")).await;
        let (r, c, t) = (root, contract.clone(), test.clone());
        let result = tokio::task::spawn_blocking(move || {
            std::process::Command::new("forge")
                .arg("test")
                .arg("--root")
                .arg(&r)
                .arg("--match-contract")
                .arg(&c)
                .arg("--match-test")
                .arg(&t)
                .arg("-vvv")
                .output()
        })
        .await;
        self.progress_end(token).await;
        match result {
            Ok(Ok(out)) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!("forge test {contract}.{test}\n{stdout}{stderr}"),
                    )
                    .await;
                let ok = out.status.success();
                let summary = test_summary(&stdout).unwrap_or_else(|| {
                    format!("{contract}.{test} {}", if ok { "passed" } else { "failed" })
                });
                self.client
                    .show_message(if ok { MessageType::INFO } else { MessageType::ERROR }, summary)
                    .await;
            }
            Ok(Err(e)) => {
                self.client
                    .show_message(MessageType::ERROR, format!("could not run `forge test`: {e}"))
                    .await;
            }
            Err(e) => {
                self.client
                    .log_message(MessageType::ERROR, format!("forge test task failed: {e}"))
                    .await;
            }
        }
    }

    /// Run work off the message loop so the server stays responsive. Debounced
    /// and coalesced per root: a burst of opens/saves in one project collapses
    /// into a single whole-project compile after editing settles, rather than
    /// queuing one compile (and one uncached `forge lint`) per file behind the
    /// compile mutex. The last event's URI wins — it only seeds the root and the
    /// fallback for location-less errors, and every file in the burst shares the
    /// same root and the same whole-project compile.
    fn schedule_diagnostics(&self, uri: Url) {
        let me = self.clone();
        tokio::spawn(async move {
            let Some(root) = uri.to_file_path().ok().and_then(|p| project::locate_root(&p)) else {
                // No Foundry root: nothing to coalesce (run_diagnostics only warns
                // for a standalone file). Preserve that by running it directly.
                me.run_diagnostics(uri).await;
                return;
            };
            let gen = {
                let mut g = me.state.diag_gen.lock().await;
                let next = g.get(&root).copied().unwrap_or(0) + 1;
                g.insert(root.clone(), next);
                next
            };
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            // A newer open/save superseded this one; let that one compile.
            if me.state.diag_gen.lock().await.get(&root).copied() != Some(gen) {
                return;
            }
            me.run_diagnostics(uri).await;
        });
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
            // A build for this root may already be running (large projects take
            // tens of seconds). `build_index` single-flights and would silently
            // drop us, losing this save. Wait for the in-flight build to finish,
            // then index the now-current sources — unless a newer save arrives
            // meanwhile, in which case that one takes over.
            loop {
                let busy = me
                    .state
                    .indexing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&root);
                if !busy {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if me.state.index_gen.lock().await.get(&root).copied() != Some(gen) {
                    return;
                }
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
        // All open buffers, keyed by path, so the check can type the edited file
        // against the *unsaved* text of any file it imports (not stale disk).
        let (buffer, open) = {
            let docs = self.state.docs.read().await;
            let Some(buffer) = docs.get(&uri).cloned() else {
                return;
            };
            let open: HashMap<PathBuf, String> = docs
                .iter()
                .filter_map(|(u, t)| u.to_file_path().ok().map(|p| (p, t.clone())))
                .collect();
            (buffer, open)
        };

        let root = project::locate_root(&path);
        let (r, t, buf) = (root.clone(), path.clone(), buffer.clone());
        // Bound concurrent solc type-checks. Each is a full-graph solc run that
        // can take one to several GB; a branch switch or `forge install` used to
        // fan one out per open tab at once, risking an out-of-memory kill. A few
        // permits keep as-you-type latency low while capping peak memory.
        let Ok(_permit) = live_check_semaphore().acquire().await else {
            return;
        };
        let errors = tokio::task::spawn_blocking(move || match r {
            Some(r) => project::check_buffer(&r, &t, &buf, &open),
            None => project::check_standalone(&t, &buf),
        })
        .await;
        let errors = match errors {
            Ok(Ok(errors)) => errors,
            Ok(Err(e)) => {
                // A solc install/compile failure (svm can't fetch solc, no
                // network, a broken toolchain) is otherwise invisible — the
                // check just drops it. Surface the first one per project so a
                // first-run failure isn't silent; standalone files (no root)
                // stay quiet to avoid noise.
                if let Some(root) = &root {
                    if self.state.warned_solc.lock().await.insert(root.clone()) {
                        self.client
                            .show_message(
                                MessageType::ERROR,
                                format!("Solidity live check failed: {e}"),
                            )
                            .await;
                    }
                }
                return;
            }
            Err(_) => return, // the check task was cancelled or panicked
        };

        // A newer edit landed while solc ran — these checks aren't mutually
        // exclusive and a large import graph takes seconds, far longer than the
        // 300ms debounce. Its own check will publish; discarding this stale
        // result keeps a slow older check from repainting just-fixed squiggles at
        // pre-edit offsets. Covers every trigger path, including the version-less
        // `schedule_live_check_now`.
        if self.state.docs.read().await.get(&uri) != Some(&buffer) {
            return;
        }

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
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Experimental: a client may turn off call-site inlay hints via
        // `initializationOptions: { experimental: { inlayHints: false } }`. When
        // off we simply don't advertise the provider, so the editor never asks.
        // Default on. Toggling takes effect on the next server start.
        let inlay_hints = params
            .initialization_options
            .as_ref()
            .and_then(|o| o.get("experimental"))
            .and_then(|e| e.get("inlayHints"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        // Whether the client accepts a dynamically-registered file watcher, so
        // `initialized` only registers one when it will actually be honored.
        let watch = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref())
            .and_then(|d| d.dynamic_registration)
            .unwrap_or(false);
        self.state.watch_dynamic_registration.store(watch, Ordering::Relaxed);
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
                code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(false) }),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![RUN_TEST_COMMAND.to_string()],
                    work_done_progress_options: Default::default(),
                }),
                inlay_hint_provider: inlay_hints.then_some(OneOf::Left(true)),
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
        // Register the file watcher over the protocol so editors that don't ship
        // a client-side watcher (Zed) still deliver foundry.toml / remappings.txt
        // / .sol changes; without it the watched-files handler never fires there.
        if self.state.watch_dynamic_registration.load(Ordering::Relaxed) {
            self.register_file_watchers().await;
        }
        // forge drives fmt/lint and (via svm) solc installs; without it those
        // features silently do nothing. Warn once at startup if it's absent.
        let has_forge = tokio::task::spawn_blocking(|| {
            std::process::Command::new("forge")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
        })
        .await
        .unwrap_or(false);
        if !has_forge {
            self.client
                .show_message(
                    MessageType::WARNING,
                    "`forge` was not found on PATH; formatting and lint are disabled. \
                     Install Foundry: https://getfoundry.sh"
                        .to_string(),
                )
                .await;
        }
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
        // Live parser fallback (cold start, mid-edit, or no foundry.toml), across
        // the whole project when the background parse is ready, else open buffers.
        Ok(self
            .with_nav_map(&uri, |m| {
                // An import path opens the imported file; relative paths resolve
                // here even without an index (remapped ones come from the index).
                if let Some(loc) = parse::import_definition(m.get(&uri), &uri, p.position) {
                    return Some(GotoDefinitionResponse::Scalar(loc));
                }
                let locs = parse::definition(m, &uri, p.position);
                (!locs.is_empty()).then_some(GotoDefinitionResponse::Array(locs))
            })
            .await)
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
                    // A resolved symbol answers definitively — even zero refs is
                    // correct and must not fall back to name matching, which would
                    // resurrect unrelated same-named occurrences. Only an
                    // unresolved cursor (None) drops to the parser below.
                    if let Some(locs) = idx.references(&path, p.position, include) {
                        return Ok((!locs.is_empty()).then_some(locs));
                    }
                }
            }
        }
        let locs = self
            .with_nav_map(&uri, |m| parse::references(m, &uri, p.position, include))
            .await;
        Ok((!locs.is_empty()).then_some(locs))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let p = params.text_document_position_params;
        let uri = p.text_document.uri;
        // Accurate solc index (resolves the symbol, not just the name) when it
        // matches the buffer; else the live tree-sitter parse.
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(idx) = guard.get(&root) {
                    let hl = idx.highlights(&path, p.position);
                    if !hl.is_empty() {
                        return Ok(Some(hl));
                    }
                }
            }
        }
        let parsed = self.state.parsed.read().await;
        let Some(file) = parsed.get(&uri) else {
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
        // Search every open project's index so workspace symbols span a monorepo,
        // then fold in the live parser for what the index can't answer for yet:
        // open buffers with unsaved edits, and files no index covers (a brand-new
        // file, or a project stuck on a compile error so its index never
        // refreshed). Short-circuiting on a non-empty index left new/unsaved
        // symbols unfindable — forever if the project won't compile.
        let query = &params.query;
        let guard = self.state.index.read().await;
        let mut out: Vec<SymbolInformation> =
            guard.values().flat_map(|idx| idx.workspace_symbols(query)).collect();
        drop(guard);

        // Files the index already answers for (by URI), and the (uri, name) pairs
        // already listed, so the parser only adds what's missing.
        let covered: HashSet<Url> = out.iter().map(|s| s.location.uri.clone()).collect();
        let mut seen: HashSet<(Url, String)> =
            out.iter().map(|s| (s.location.uri.clone(), s.name.clone())).collect();

        // Open buffers whose unsaved text the saved-snapshot index hasn't indexed.
        let open: Vec<Url> = self.state.parsed.read().await.keys().cloned().collect();
        let mut dirty: HashSet<Url> = HashSet::new();
        for uri in open {
            if self.is_dirty(&uri).await {
                dirty.insert(uri);
            }
        }

        // Live-parser candidates: every project source via the whole-project
        // background parses (so a root whose index isn't ready — cold start,
        // broken build — is still searchable), plus the open buffers (for a
        // standalone file, or one no root covers). Open buffers appear in both and
        // are deduped by (uri, name).
        let mut candidates: Vec<SymbolInformation> = Vec::new();
        {
            let ws = self.state.workspace_parsed.read().await;
            for map in ws.values() {
                candidates.extend(parse::workspace_symbols(map, query));
            }
        }
        {
            let parsed = self.state.parsed.read().await;
            candidates.extend(parse::workspace_symbols(&parsed, query));
        }
        for s in candidates {
            let uri = &s.location.uri;
            // Skip files the index covers unless they have unsaved edits.
            if covered.contains(uri) && !dirty.contains(uri) {
                continue;
            }
            if seen.insert((uri.clone(), s.name.clone())) {
                out.push(s);
            }
        }
        Ok(Some(out))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let p = params.text_document_position;
        let uri = p.text_document.uri;
        let Some(text) = self.state.docs.read().await.get(&uri).cloned() else {
            return Ok(None);
        };
        let offset = diagnostics::PositionMapper::new(&text).offset(p.position);

        // Import path: sibling files/dirs relative to this file, plus the
        // project's remapping prefixes. No index or parse needed, but resolving
        // remappings walks lib/ and reading the directory hits disk, so run it
        // off the dispatch task like every other heavy path.
        if let Some(prefix) = complete::import_path_context(&text, offset) {
            let uri = uri.clone();
            let items = tokio::task::spawn_blocking(move || {
                let path = uri.to_file_path().ok();
                let dir = path.as_deref().and_then(Path::parent).map(Path::to_path_buf);
                let remaps = path
                    .as_deref()
                    .and_then(project::locate_root)
                    .map(|r| project::remapping_prefixes(&r))
                    .unwrap_or_default();
                dir.map(|d| complete::import_completions(&d, &prefix, &remaps)).unwrap_or_default()
            })
            .await
            .unwrap_or_default();
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
                    let more =
                        self.with_nav_map(&uri, |m| parse::member_completions(m, c)).await;
                    items.extend(more);
                }
            }
            None => {
                // Symbols in scope (accurate index when valid, else the parser).
                let symbols = if let Some(root) = self.valid_index_root(&uri).await {
                    self.state
                        .index
                        .read()
                        .await
                        .get(&root)
                        .map(|idx| idx.global_completions())
                        .unwrap_or_default()
                } else {
                    let parsed = self.state.parsed.read().await;
                    parse::global_completions(&parsed)
                };
                // Merge with snippets, builtins and keywords, collapsing duplicate
                // labels — VS Code only merges items sharing both label and kind,
                // so the `contract` keyword and `contract` snippet would otherwise
                // both show. A snippet or builtin outranks the bare keyword of the
                // same name; a stable per-group sort_text keeps the ordering fixed.
                items = dedup_completions(vec![
                    symbols,
                    complete::snippets(),
                    complete::global_builtins(),
                    complete::keywords(),
                ]);
            }
        }
        Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let p = params.text_document_position_params;
        let uri = p.text_document.uri;
        let Some(text) = self.state.docs.read().await.get(&uri).cloned() else {
            return Ok(None);
        };
        let offset = diagnostics::PositionMapper::new(&text).offset(p.position);
        let Some((callee, active)) = call_context(&text, offset) else {
            return Ok(None);
        };
        // Accurate index, resolved by callee name — position-independent, so no
        // buffer-match gate; signature help should keep working mid-edit.
        if let Some(root) = uri.to_file_path().ok().and_then(|path| project::locate_root(&path)) {
            let guard = self.state.index.read().await;
            if let Some(help) = guard.get(&root).and_then(|idx| idx.signatures(&callee, active)) {
                return Ok(Some(help));
            }
        }
        // Live parser fallback: cold start, a broken build, or no foundry.toml,
        // where the index has nothing to answer with.
        let parsed = self.state.parsed.read().await;
        Ok(parse::signatures(&parsed, &callee, active))
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let req = params.range;
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        // `forge lint` suggested replacements (from the last compile). Their byte
        // ranges are the on-disk text's, so once the buffer has unsaved edits
        // applying one would splice at a shifted position. Skip them while dirty
        // (as diagnostics/semantic tokens already do); they reappear on save.
        if !self.is_dirty(&uri).await {
            if let Some(file_fixes) = self.state.fixes.lock().await.get(&uri) {
                for f in file_fixes.iter().filter(|f| ranges_overlap(f.range, req)) {
                    actions.push(quickfix(&uri, f.range, f.new_text.clone(), f.title.clone()));
                }
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

                // Mechanical single-edit fixes for common solc error codes
                // (visibility, data location, override/virtual, abstract,
                // checksummed address, mutability). Each self-guards on the code
                // and only fires when the edit is unambiguous.
                if let Some((range, new_text, title)) = solc_quickfix(d, &text) {
                    actions.push(quickfix(&uri, range, new_text, title));
                }
            }
        }

        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        // Only Foundry test files carry run-test lenses.
        if !uri.path().ends_with(".t.sol") {
            return Ok(None);
        }
        let parsed = self.state.parsed.read().await;
        let Some(file) = parsed.get(&uri) else {
            return Ok(None);
        };
        let lenses: Vec<CodeLens> = parse::test_lenses(file)
            .into_iter()
            .map(|t| CodeLens {
                range: t.range,
                command: Some(Command {
                    title: "\u{25b6} Run test".to_string(),
                    command: RUN_TEST_COMMAND.to_string(),
                    arguments: Some(vec![
                        serde_json::Value::String(uri.to_string()),
                        serde_json::Value::String(t.contract),
                        serde_json::Value::String(t.function),
                    ]),
                }),
                data: None,
            })
            .collect();
        Ok((!lenses.is_empty()).then_some(lenses))
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command != RUN_TEST_COMMAND {
            return Ok(None);
        }
        let arg = |i: usize| params.arguments.get(i).and_then(|v| v.as_str()).map(str::to_string);
        let (Some(uri), Some(contract), Some(test)) = (arg(0), arg(1), arg(2)) else {
            return Ok(None);
        };
        let root = Url::parse(&uri)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .and_then(|p| project::locate_root(&p));
        match root {
            Some(root) => self.run_forge_test(root, contract, test).await,
            None => {
                self.client
                    .show_message(
                        MessageType::ERROR,
                        "No foundry.toml found for this test".to_string(),
                    )
                    .await;
            }
        }
        Ok(None)
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        // Only recolor from the index when its positions exactly match the live
        // buffer. On any divergence (mid-edit, cold start) return None so the
        // editor keeps its TextMate grammar instead of painting the precomputed
        // tokens onto identifiers that have since shifted.
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
        let data = idx.semantic_tokens(&path);
        Ok((!data.is_empty())
            .then_some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data })))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let range = params.range;
        // Accurate, cross-file hints from the index when it matches the buffer.
        if let Some(root) = self.valid_index_root(&uri).await {
            if let Ok(path) = uri.to_file_path() {
                let guard = self.state.index.read().await;
                if let Some(idx) = guard.get(&root) {
                    let hints = idx.inlay_hints(&path, range);
                    if !hints.is_empty() {
                        return Ok(Some(hints));
                    }
                }
            }
        }
        // Live parser fallback: tracks the buffer while typing, before the index
        // is in sync (cold start, just-edited), and through compile errors.
        let parsed = self.state.parsed.read().await;
        let hints = parse::call_hints(&parsed, &uri, range);
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
                if let Some(idx) = guard.get(&root) {
                    // Don't offer rename for a symbol declared in a dependency.
                    let vendored = idx
                        .declaration_path(&path, params.position)
                        .is_some_and(|d| under_libs(&root, &d));
                    if !vendored {
                        if let Some(range) = idx.rename_range(&path, params.position) {
                            return Ok(Some(PrepareRenameResponse::Range(range)));
                        }
                    }
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
        // Refuse to edit vendored dependency sources under the project's libs.
        if idx.declaration_path(&path, p.position).is_some_and(|d| under_libs(&root, &d)) {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(
                "cannot rename a symbol declared in a library dependency".to_string(),
            ));
        }
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
        // Mirror into the whole-project map (if its root is already parsed) before
        // moving the parse into the open-buffer map.
        self.sync_workspace_buffer(&doc.uri, file.clone()).await;
        self.state.parsed.write().await.insert(doc.uri.clone(), file);
        // Fast type-check first (instant feedback), then the full codegen
        // compile + navigation index in the background.
        self.schedule_live_check_now(doc.uri.clone());
        self.schedule_diagnostics(doc.uri.clone());
        // Only (re)index when this file isn't already covered by a current index.
        // At open the buffer equals disk, so a matching index is exactly valid and
        // a fresh cold full compile would just burn CPU — painful when browsing a
        // large project file by file.
        if self.valid_index_root(&doc.uri).await.is_none() {
            self.schedule_index(doc.uri.clone());
        }
        // Parse the whole owning project in the background the first time we see
        // its root, so navigation spans it even before any compile succeeds.
        if let Some(root) = doc.uri.to_file_path().ok().and_then(|p| project::locate_root(&p)) {
            self.schedule_workspace_parse(root);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        self.state.live_versions.lock().await.insert(uri.clone(), version);
        // FULL sync: the last change carries the entire document text. Store it
        // synchronously so diagnostics and index-validity see the current text at
        // once, then re-parse off the message loop — a from-scratch tree-sitter
        // parse of a large file would otherwise stall stdin/stdout and queued
        // requests on every keystroke. A version guard discards an out-of-order
        // parse so a slower earlier one can't clobber a newer tree.
        if let Some(change) = params.content_changes.into_iter().next_back() {
            self.state.docs.write().await.insert(uri.clone(), change.text.clone());
            let me = self.clone();
            let (u, text) = (uri.clone(), change.text);
            tokio::spawn(async move {
                let Ok(file) = tokio::task::spawn_blocking(move || parse::parse(&text)).await
                else {
                    return;
                };
                if me.state.live_versions.lock().await.get(&u).copied() == Some(version) {
                    me.sync_workspace_buffer(&u, file.clone()).await;
                    me.state.parsed.write().await.insert(u, file);
                }
            });
        }
        self.schedule_live_check(uri, version);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.schedule_diagnostics(params.text_document.uri.clone());
        self.schedule_index(params.text_document.uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        // Was the buffer dirty? Its live squiggles point into text that closing
        // discards, so they'd linger as ghosts at offsets no file has. Check
        // before dropping the buffer, since is_dirty reads it.
        let was_dirty = self.is_dirty(&uri).await;
        self.state.docs.write().await.remove(&uri);
        self.state.parsed.write().await.remove(&uri);
        // Drop the version entry: it would otherwise grow unbounded (one per
        // edited file), and a did_change parse task still in flight would pass
        // its version guard and reinsert the closed doc into `parsed`, leaving a
        // ghost the parser-based features keep answering from. Removing it makes
        // that guard fail (None != Some(version)).
        self.state.live_versions.lock().await.remove(&uri);
        if was_dirty {
            // The whole-project map mirrored the now-discarded edits; re-parse the
            // file from disk so project-wide navigation reflects the saved truth.
            self.refresh_workspace_file(uri.clone());
            // Clear the stale diagnostics, then restore the on-disk truth: a
            // Foundry file gets a fresh compile (which republishes any real
            // on-disk errors); a standalone file has nothing to recompile.
            self.state.published.lock().await.remove(&uri);
            self.client.publish_diagnostics(uri.clone(), Vec::new(), None).await;
            if uri.to_file_path().ok().and_then(|p| project::locate_root(&p)).is_some() {
                self.schedule_diagnostics(uri);
            }
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // foundry.toml / remappings.txt edits change a project's config (solc
        // version, remappings, source layout), and a .sol file can change on disk
        // outside the editor (forge install, branch switch). Skip events for .sol
        // files we already have open — did_change / did_save own those, and
        // re-triggering here would double-compile.
        let open: Vec<Url> = self.state.docs.read().await.keys().cloned().collect();
        let mut roots: HashSet<PathBuf> = HashSet::new();
        for change in &params.changes {
            let Ok(path) = change.uri.to_file_path() else {
                continue;
            };
            let is_sol = path.extension().and_then(|e| e.to_str()) == Some("sol");
            if is_sol && open.contains(&change.uri) {
                continue;
            }
            if let Some(root) = project::locate_root(&path) {
                roots.insert(root);
            }
        }
        if roots.is_empty() {
            return;
        }
        // Drop each affected root's memoized config + remappings: a forge install
        // or branch switch can add or remove lib/ sources (changing remappings)
        // without editing foundry.toml or remappings.txt, so the content-keyed
        // memo wouldn't otherwise notice.
        for root in &roots {
            project::invalidate_root(root);
            // Re-arm the whole-project background parse: a branch switch or forge
            // install can add, remove or edit .sol files outside any open buffer,
            // which the mirrored open-buffer syncs wouldn't otherwise pick up.
            self.state.workspace_roots.lock().await.remove(root);
            self.schedule_workspace_parse(root.clone());
        }
        // One compile + index per affected root, keyed off any open buffer in it.
        // The warm incremental compile republishes and clears diagnostics across
        // the whole project — open and unopened files alike, so files a branch
        // switch fixed stop showing stale squiggles. Doing this once per root,
        // instead of a live type-check per open tab, avoids running a dozen solc
        // processes at once (a memory blowout the old fan-out risked).
        let mut done: HashSet<PathBuf> = HashSet::new();
        for uri in &open {
            let Ok(p) = uri.to_file_path() else {
                continue;
            };
            let Some(root) = project::locate_root(&p) else {
                continue;
            };
            if roots.contains(&root) && done.insert(root) {
                self.schedule_diagnostics(uri.clone());
                self.schedule_index(uri.clone());
            }
        }
    }
}

/// Globally bounds concurrent as-you-type solc type-checks (`live_check`) across
/// every open buffer, so a burst — a branch switch or `forge install` touching
/// many open tabs — can't launch one heavyweight solc process per file at once.
fn live_check_semaphore() -> &'static tokio::sync::Semaphore {
    static SEM: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SEM.get_or_init(|| tokio::sync::Semaphore::new(3))
}

/// A one-line result from `forge test` output: the per-test `[PASS]`/`[FAIL…]`
/// line, else the suite-result line, for a concise pass/fail popup.
fn test_summary(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("[PASS]") || l.starts_with("[FAIL"))
        .or_else(|| stdout.lines().map(str::trim).find(|l| l.starts_with("Suite result")))
        .map(str::to_string)
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

/// The contents of the first double-quoted run in a message, if any. solc names
/// the concrete token it suggests (a pragma, a visibility, a checksummed address)
/// verbatim between double quotes.
fn quoted_str(msg: &str) -> Option<&str> {
    let start = msg.find('"')? + 1;
    let end = msg[start..].find('"')? + start;
    Some(&msg[start..end])
}

/// Pull the suggested pragma out of solc's "does not specify required compiler
/// version" message, which names it verbatim in quotes.
fn pragma_from_message(msg: &str) -> Option<String> {
    let candidate = quoted_str(msg)?.trim();
    candidate.starts_with("pragma").then(|| candidate.to_string())
}

/// A mechanical, unambiguous single-edit quick-fix for a common solc error,
/// derived only from the diagnostic's code, message and range plus the current
/// buffer. Returns `(range, replacement, title)`; `None` when the code isn't one
/// we fix or the fix would be ambiguous. Each arm keeps the edit trivially
/// correct (an order-independent specifier, a keyword insertion, a verbatim
/// replacement) rather than guessing at anything solc didn't spell out.
fn solc_quickfix(d: &Diagnostic, text: &str) -> Option<(Range, String, String)> {
    let code = match &d.code {
        Some(NumberOrString::Number(n)) => *n,
        _ => return None,
    };
    match code {
        9429 => fix_checksum(d, text),
        3656 => fix_abstract(d, text),
        6651 => fix_data_location(d, text),
        4937 => fix_visibility(d, text),
        2018 => fix_mutability(d, text),
        9456 => fix_specifier(d, text, "override", "Add `override` specifier"),
        4334 => fix_specifier(d, text, "virtual", "Add `virtual` specifier"),
        _ => None,
    }
}

/// A zero-width insertion of `ins` at byte `at`, with the given title.
fn insertion(text: &str, at: usize, ins: &str, title: String) -> (Range, String, String) {
    let pos = diagnostics::PositionMapper::new(text).position(at);
    (Range::new(pos, pos), ins.to_string(), title)
}

/// Byte offset just past the closing `)` of the parameter list of a function
/// whose declaration begins at or after `from`. Scans for the first `(` — bailing
/// at a `{`, `;` or `=` so a declaration with no parameter list (e.g. a state
/// variable) yields `None` — then matches nesting to its close. Solidity accepts
/// visibility / mutability / `override` / `virtual` in any order, so inserting a
/// specifier right here is always well-formed.
fn after_param_list(text: &str, from: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = from.min(b.len());
    while i < b.len() {
        match b[i] {
            b'(' => break,
            b'{' | b';' | b'=' => return None,
            _ => i += 1,
        }
    }
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The function-header text between `from` and the body `{` or `;`, so a caller
/// can check which specifiers are already present.
fn header_specifiers(text: &str, from: usize) -> &str {
    let b = text.as_bytes();
    let from = from.min(b.len());
    let mut i = from;
    while i < b.len() && b[i] != b'{' && b[i] != b';' {
        i += 1;
    }
    &text[from..i]
}

/// Byte offset of `word` in `hay` as a whole token (not a substring of a larger
/// identifier), if present.
fn find_word(hay: &str, word: &str) -> Option<usize> {
    let b = hay.as_bytes();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(word) {
        let i = from + rel;
        let before = i == 0 || !is_ident_byte(b[i - 1]);
        let after = i + word.len() >= b.len() || !is_ident_byte(b[i + word.len()]);
        if before && after {
            return Some(i);
        }
        from = i + 1;
    }
    None
}

/// Where and how to splice a new function specifier (`kw`) so it reads in
/// conventional order — after any existing visibility/mutability/modifiers, just
/// before the return clause or body — with clean single-space padding regardless
/// of the surrounding whitespace. `param_close` is the byte just past the
/// parameter list. Returns the insertion `(byte, text)`.
fn splice_specifier(text: &str, param_close: usize, kw: &str) -> (usize, String) {
    let b = text.as_bytes();
    let mut end = param_close.min(b.len());
    while end < b.len() && b[end] != b'{' && b[end] != b';' {
        end += 1;
    }
    let point = match find_word(&text[param_close..end], "returns") {
        Some(rel) => param_close + rel,
        None => end,
    };
    let lead = point > 0 && b.get(point - 1).is_some_and(|c| !c.is_ascii_whitespace());
    let trail = b.get(point).is_some_and(|c| !c.is_ascii_whitespace());
    let ins = format!("{}{kw}{}", if lead { " " } else { "" }, if trail { " " } else { "" });
    (point, ins)
}

/// Insert an order-independent function specifier (`override` / `virtual`) into
/// the header of the declaration at the diagnostic's location.
fn fix_specifier(d: &Diagnostic, text: &str, kw: &str, title: &str) -> Option<(Range, String, String)> {
    let start = diagnostics::PositionMapper::new(text).offset(d.range.start);
    let at = after_param_list(text, start)?;
    let (point, ins) = splice_specifier(text, at, kw);
    Some(insertion(text, point, &ins, title.to_string()))
}

/// 4937 — insert the visibility solc names after the parameter list.
fn fix_visibility(d: &Diagnostic, text: &str) -> Option<(Range, String, String)> {
    let vis = quoted_str(&d.message)?;
    if !matches!(vis, "public" | "private" | "internal" | "external") {
        return None;
    }
    fix_specifier(d, text, vis, &format!("Add `{vis}` visibility"))
}

/// 2018 — restrict a function to `view`/`pure` (solc names which) by inserting the
/// keyword. Skips the case where a mutability specifier is already present (a
/// `view` that could be `pure`), which would need a replacement, not an insert.
fn fix_mutability(d: &Diagnostic, text: &str) -> Option<(Range, String, String)> {
    let kw = d.message.rsplit(' ').next().filter(|w| matches!(*w, "view" | "pure"))?;
    let start = diagnostics::PositionMapper::new(text).offset(d.range.start);
    let at = after_param_list(text, start)?;
    if header_specifiers(text, at)
        .split_whitespace()
        .any(|w| matches!(w, "view" | "pure" | "payable"))
    {
        return None;
    }
    fix_specifier(d, text, kw, &format!("Restrict mutability to `{kw}`"))
}

/// 3656 — mark a contract `abstract` by inserting the keyword before `contract`.
fn fix_abstract(d: &Diagnostic, text: &str) -> Option<(Range, String, String)> {
    let start = diagnostics::PositionMapper::new(text).offset(d.range.start);
    let rest = text.get(start..)?;
    // The diagnostic must sit on the `contract` keyword itself.
    if !rest.starts_with("contract") || rest.as_bytes().get(8).copied().is_some_and(is_ident_byte) {
        return None;
    }
    Some(insertion(text, start, "abstract ", "Mark contract as `abstract`".to_string()))
}

/// 9429 — replace an address literal with the checksummed form solc names.
fn fix_checksum(d: &Diagnostic, text: &str) -> Option<(Range, String, String)> {
    let addr = quoted_str(&d.message)?;
    if !addr.starts_with("0x") || addr.len() != 42 || !addr[2..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    // Only when the diagnostic is actually on an address literal.
    if !slice(text, d.range).starts_with("0x") {
        return None;
    }
    Some((d.range, addr.to_string(), format!("Convert to checksummed address `{addr}`")))
}

/// 6651 — insert a `memory` data location between a reference-type parameter/local
/// and its name. `memory` is valid everywhere solc reports this in 0.8 (function
/// parameters and locals), so it's the one unambiguous single-keyword fix;
/// mappings (storage-only) and declarations that already name a location or have
/// no variable name are left alone.
fn fix_data_location(d: &Diagnostic, text: &str) -> Option<(Range, String, String)> {
    let m = diagnostics::PositionMapper::new(text);
    let start = m.offset(d.range.start);
    let end = m.offset(d.range.end);
    let decl = text.get(start..end)?;
    if decl.contains("mapping")
        || decl.split_whitespace().any(|w| matches!(w, "memory" | "storage" | "calldata"))
    {
        return None;
    }
    // The variable name is the trailing identifier; insert `memory` before it.
    let b = text.as_bytes();
    let mut name = end;
    while name > start && b[name - 1].is_ascii_whitespace() {
        name -= 1;
    }
    let name_end = name;
    while name > start && is_ident_byte(b[name - 1]) {
        name -= 1;
    }
    // Require both a trailing name and a preceding type.
    if name == name_end || name == start {
        return None;
    }
    Some(insertion(text, name, "memory ", "Add `memory` data location".to_string()))
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

/// Whether `decl_path` lives under one of `root`'s library directories — a
/// vendored dependency source a rename must not silently edit.
fn under_libs(root: &Path, decl_path: &Path) -> bool {
    project::lib_dirs(root).iter().any(|lib| {
        let lib = std::fs::canonicalize(lib).unwrap_or_else(|_| lib.clone());
        decl_path.starts_with(&lib)
    })
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

/// Concatenate completion groups in priority order, dropping any later item
/// whose label was already taken (so a richer snippet/builtin wins over the bare
/// keyword of the same name), and stamp each with a `sort_text` that preserves
/// the group order in the editor's list.
fn dedup_completions(groups: Vec<Vec<CompletionItem>>) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (bucket, group) in groups.into_iter().enumerate() {
        for mut item in group {
            if seen.insert(item.label.clone()) {
                item.sort_text = Some(format!("{bucket}{}", item.label));
                out.push(item);
            }
        }
    }
    out
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
        relative_import, solc_quickfix, under_libs, valid_new_name,
    };
    use crate::diagnostics::PositionMapper;
    use std::path::Path;
    use tower_lsp::lsp_types::{Diagnostic, NumberOrString, Position, Range};

    /// Build a diagnostic carrying `code`, `message` and a range covering the
    /// byte span `(start, end)` of `text`, for exercising `solc_quickfix`.
    fn diag(code: i32, message: &str, text: &str, span: (usize, usize)) -> Diagnostic {
        let m = PositionMapper::new(text);
        Diagnostic {
            range: Range::new(m.position(span.0), m.position(span.1)),
            code: Some(NumberOrString::Number(code)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    /// Splice a single quick-fix edit into `text`, to assert the resulting source.
    fn apply_fix(text: &str, range: Range, new_text: &str) -> String {
        let m = PositionMapper::new(text);
        let (s, e) = (m.offset(range.start), m.offset(range.end));
        format!("{}{}{}", &text[..s], new_text, &text[e..])
    }

    #[test]
    fn rename_refuses_library_sources() {
        // Default libs is `lib`: a declaration there is a vendored dependency and
        // must not be renamed; project sources are fine.
        let root = Path::new("/proj");
        assert!(under_libs(root, Path::new("/proj/lib/oz/contracts/Ownable.sol")));
        assert!(!under_libs(root, Path::new("/proj/src/MyToken.sol")));
    }

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

    #[test]
    fn quickfix_checksum_replaces_address_literal() {
        let text = "address a = 0x52908400098527886E0F7030069857d2E4169EE7;";
        let lit = text.find("0x").unwrap();
        let msg = "This looks like an address but has an invalid checksum. Correct \
                   checksummed address: \"0x52908400098527886E0F7030069857D2E4169EE7\". \
                   If this is not used as an address, please prepend '00'.";
        let (range, nt, _) =
            solc_quickfix(&diag(9429, msg, text, (lit, lit + 42)), text).unwrap();
        assert_eq!(
            apply_fix(text, range, &nt),
            "address a = 0x52908400098527886E0F7030069857D2E4169EE7;"
        );
        // Skip: the message names no address to substitute.
        assert!(solc_quickfix(&diag(9429, "no address here", text, (lit, lit + 42)), text).is_none());
    }

    #[test]
    fn quickfix_visibility_inserts_after_param_list() {
        let text = "function f() returns (uint) { return 1; }";
        let span = (0, text.len());
        let msg = "No visibility specified. Did you intend to add \"public\"?";
        let (range, nt, _) = solc_quickfix(&diag(4937, msg, text, span), text).unwrap();
        assert_eq!(apply_fix(text, range, &nt), "function f() public returns (uint) { return 1; }");
        // Skip: the quoted word isn't a visibility keyword.
        assert!(solc_quickfix(&diag(4937, "add \"foo\"?", text, span), text).is_none());
    }

    #[test]
    fn quickfix_mutability_inserts_or_skips() {
        let text = "function f() public returns (uint) { return s; }";
        let (range, nt, _) = solc_quickfix(
            &diag(2018, "Function state mutability can be restricted to view", text, (0, text.len())),
            text,
        )
        .unwrap();
        assert_eq!(apply_fix(text, range, &nt), "function f() public view returns (uint) { return s; }");
        // Skip: a function that already has `view` would need a replacement, not
        // an insertion, to become `pure`.
        let v = "function f() public view returns (uint) { return 1; }";
        assert!(solc_quickfix(
            &diag(2018, "Function state mutability can be restricted to pure", v, (0, v.len())),
            v,
        )
        .is_none());
    }

    #[test]
    fn quickfix_override_and_virtual_insert_specifier() {
        let text = "function foo() public returns (uint) { return 2; }";
        let span = (0, text.len());
        let (r, nt, _) = solc_quickfix(
            &diag(9456, "Overriding function is missing \"override\" specifier.", text, span),
            text,
        )
        .unwrap();
        assert_eq!(apply_fix(text, r, &nt), "function foo() public override returns (uint) { return 2; }");
        let (r, nt, _) = solc_quickfix(
            &diag(4334, "Trying to override non-virtual function. Did you forget to add \"virtual\"?", text, span),
            text,
        )
        .unwrap();
        assert_eq!(apply_fix(text, r, &nt), "function foo() public virtual returns (uint) { return 2; }");
        // Skip: a state-variable override has no parameter list to anchor on.
        let sv = "uint256 public x;";
        assert!(solc_quickfix(&diag(9456, "", sv, (0, sv.len())), sv).is_none());
    }

    #[test]
    fn quickfix_abstract_prefixes_contract_keyword() {
        let text = "contract T is I {}";
        let (r, nt, _) = solc_quickfix(
            &diag(3656, "Contract \"T\" should be marked as abstract.", text, (0, text.len())),
            text,
        )
        .unwrap();
        assert_eq!(apply_fix(text, r, &nt), "abstract contract T is I {}");
        // Skip: the diagnostic doesn't start on the `contract` keyword.
        let off = "  x contract T {}";
        assert!(solc_quickfix(&diag(3656, "", off, (0, off.len())), off).is_none());
    }

    #[test]
    fn quickfix_data_location_inserts_memory() {
        let text = "string x";
        let (r, nt, _) = solc_quickfix(
            &diag(6651, "Data location must be \"memory\" or \"calldata\" for parameter in function, but none was given.", text, (0, text.len())),
            text,
        )
        .unwrap();
        assert_eq!(apply_fix(text, r, &nt), "string memory x");
        // An array type, likewise.
        let arr = "uint[] x";
        let (r, nt, _) = solc_quickfix(&diag(6651, "Data location must be ...", arr, (0, arr.len())), arr).unwrap();
        assert_eq!(apply_fix(arr, r, &nt), "uint[] memory x");
        // Skip: a mapping can only live in storage.
        let map = "mapping(uint => uint) x";
        assert!(solc_quickfix(&diag(6651, "", map, (0, map.len())), map).is_none());
    }

    #[test]
    fn test_summary_extracts_result_line() {
        let pass = "Compiling...\nRan 1 test\n[PASS] test_Inc() (gas: 100)\nSuite result: ok. 1 passed";
        assert_eq!(super::test_summary(pass).as_deref(), Some("[PASS] test_Inc() (gas: 100)"));
        let fail = "[FAIL. Reason: assertion failed] test_X() (gas: 200)\n";
        assert_eq!(
            super::test_summary(fail).as_deref(),
            Some("[FAIL. Reason: assertion failed] test_X() (gas: 200)")
        );
        // No per-test line: fall back to the suite-result line.
        let suite = "Ran 0 tests\nSuite result: ok. 0 passed; 0 failed";
        assert_eq!(super::test_summary(suite).as_deref(), Some("Suite result: ok. 0 passed; 0 failed"));
        // Nothing recognizable yields None (the caller then reports pass/fail).
        assert_eq!(super::test_summary("Compiling...\n"), None);
    }

    #[test]
    fn dedup_completions_collapses_labels_and_orders_groups() {
        use tower_lsp::lsp_types::CompletionItem;
        let mk = |l: &str| CompletionItem { label: l.into(), ..Default::default() };
        let out = super::dedup_completions(vec![
            vec![mk("Foo")],                     // 0: in-scope symbol
            vec![mk("contract")],                // 1: snippet
            vec![mk("contract"), mk("require")], // 2: builtins (dup `contract` dropped)
            vec![mk("contract"), mk("public")],  // 3: keywords (dup `contract` dropped)
        ]);
        let labels: Vec<_> = out.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["Foo", "contract", "require", "public"]);
        // sort_text encodes the surviving group, so the list order is stable.
        let st = |l: &str| out.iter().find(|i| i.label == l).unwrap().sort_text.clone().unwrap();
        assert!(st("Foo").starts_with('0'));
        assert!(st("contract").starts_with('1')); // the snippet won the label
        assert!(st("require").starts_with('2'));
        assert!(st("public").starts_with('3'));
    }
}
