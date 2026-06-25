use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

struct SolidityExtension {
    /// Path to a downloaded server, cached for the session.
    cached: Option<String>,
}

impl SolidityExtension {
    /// Resolve the server binary: prefer one on PATH (a `cargo install` build),
    /// otherwise download the binary from the latest GitHub release so users
    /// don't need a Rust toolchain. Editors share the same server, so behavior
    /// matches across all of them.
    fn server_path(&mut self, id: &LanguageServerId, worktree: &Worktree) -> Result<String> {
        if let Some(path) = worktree.which("solidity-lsp") {
            return Ok(path);
        }
        if let Some(path) = &self.cached {
            if std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok(path.clone());
            }
        }

        zed::set_language_server_installation_status(
            id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let release = zed::latest_github_release(
            "u-zzam/solidity",
            zed::GithubReleaseOptions { require_assets: true, pre_release: false },
        )?;

        let (os, arch) = zed::current_platform();
        let triple = match (os, arch) {
            (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
            (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
            (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
            (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
            _ => return Err(format!("unsupported platform: {os:?}/{arch:?}")),
        };
        let exe = if matches!(os, zed::Os::Windows) { ".exe" } else { "" };
        let asset_name = format!("solidity-lsp-{triple}{exe}");
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("no release asset named {asset_name}"))?;

        let dir = format!("solidity-lsp-{}", release.version);
        let path = format!("{dir}/{asset_name}");
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
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
                    if name.starts_with("solidity-lsp-") && name != dir {
                        std::fs::remove_dir_all(entry.path()).ok();
                    }
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
