use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

struct SolidityExtension;

impl zed::Extension for SolidityExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        // Resolve the server from PATH (installed via `cargo install` or a
        // release binary). Editors share the same server, so behavior matches.
        let path = worktree
            .which("solidity-lsp")
            .ok_or_else(|| "solidity-lsp not found on PATH".to_string())?;
        Ok(Command {
            command: path,
            args: vec![],
            env: worktree.shell_env(),
        })
    }
}

zed::register_extension!(SolidityExtension);
