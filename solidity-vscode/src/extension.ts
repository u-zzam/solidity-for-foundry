import { workspace, ExtensionContext } from "vscode";
import {
  Executable,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export function activate(_context: ExtensionContext): void {
  const command =
    workspace.getConfiguration("solidity").get<string>("serverPath") ||
    "solidity-lsp";

  const run: Executable = { command };
  const serverOptions: ServerOptions = { run, debug: run };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "solidity" }],
    synchronize: {
      fileEvents: workspace.createFileSystemWatcher("**/*.sol"),
    },
  };

  client = new LanguageClient(
    "solidity",
    "Solidity (Foundry)",
    serverOptions,
    clientOptions,
  );
  client.start();
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
