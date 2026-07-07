use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

struct SolidityExtension {
    /// Path to a downloaded server, cached for the session.
    cached: Option<String>,
}

impl SolidityExtension {
    /// Resolve the server binary: prefer one on PATH (a `cargo install` build),
    /// otherwise download the binary from this extension's own GitHub release so
    /// users don't need a Rust toolchain. Editors share the same server, so behavior
    /// matches across all of them.
    fn server_path(&mut self, id: &LanguageServerId, worktree: &Worktree) -> Result<String> {
        if let Some(path) = worktree.which("solidity-for-foundry-lsp") {
            return Ok(path);
        }
        if let Some(path) = &self.cached {
            if std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok(path.clone());
            }
        }

        let (os, arch) = zed::current_platform();
        let triple = match (os, arch) {
            (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
            (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
            (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
            (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
            (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
            _ => return Err(format!("unsupported platform: {os:?}/{arch:?}")),
        };
        let exe = if matches!(os, zed::Os::Windows) { ".exe" } else { "" };
        let asset_name = format!("solidity-for-foundry-lsp-{triple}{exe}");

        // The extension only ever runs the server built for its own version, so
        // both the download tag and the on-disk cache dir key off CARGO_PKG_VERSION.
        let dir = concat!("solidity-for-foundry-lsp-", env!("CARGO_PKG_VERSION"));
        let path = format!("{dir}/{asset_name}");

        // Prefer the already-downloaded binary before touching the network, so an
        // offline machine or a rate-limited GitHub API can't kill a working server.
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            self.cached = Some(path.clone());
            return Ok(path);
        }

        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let release = zed::github_release_by_tag_name(
            "u-zzam/solidity-for-foundry",
            concat!("v", env!("CARGO_PKG_VERSION")),
        )?;
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("no release asset named {asset_name}"))?;

        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );
        zed::download_file(&asset.download_url, &path, zed::DownloadedFileType::Uncompressed)?;
        zed::make_file_executable(&path)?;
        // Drop older downloaded versions.
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("solidity-for-foundry-lsp-") && name != dir {
                    std::fs::remove_dir_all(entry.path()).ok();
                }
            }
        }

        self.cached = Some(path.clone());
        Ok(path)
    }
}

impl zed::Extension for SolidityExtension {
    fn new() -> Self {
        Self { cached: None }
    }

    fn language_server_command(
        &mut self,
        id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let command = self.server_path(id, worktree)?;
        Ok(Command { command, args: vec![], env: worktree.shell_env() })
    }
}

zed::register_extension!(SolidityExtension);
