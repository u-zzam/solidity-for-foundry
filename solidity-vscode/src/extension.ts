import {
  workspace,
  window,
  commands,
  ExtensionContext,
  ProgressLocation,
} from "vscode";
import {
  Executable,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";
import * as fs from "fs";
import * as https from "https";
import * as path from "path";

let client: LanguageClient | undefined;

const REPO = "u-zzam/solidity-for-foundry";

/// The release asset's Rust target triple for the current platform.
function targetTriple(): string | undefined {
  const key = `${process.platform}-${process.arch}`;
  return {
    "darwin-arm64": "aarch64-apple-darwin",
    "darwin-x64": "x86_64-apple-darwin",
    "linux-x64": "x86_64-unknown-linux-gnu",
    "linux-arm64": "aarch64-unknown-linux-gnu",
    "win32-x64": "x86_64-pc-windows-msvc",
  }[key];
}

/// Download `url` to `dest`, following GitHub's redirect to the asset storage.
function download(url: string, dest: string): Promise<void> {
  return new Promise((resolve, reject) => {
    // Stream into a sibling .part file and rename only once it is fully written,
    // so a process kill mid-download can never leave a truncated binary at `dest`
    // that the next launch trusts via existsSync.
    const part = `${dest}.part`;
    const file = fs.createWriteStream(part);
    const fail = (e: Error) => {
      file.destroy();
      fs.rm(part, () => reject(e));
    };
    file.on("error", fail); // disk write failure (full disk, permissions)
    const get = (u: string, redirects: number) => {
      if (redirects > 5) {
        fail(new Error("too many redirects"));
        return;
      }
      const req = https.get(
        u,
        { headers: { "User-Agent": "solidity-vscode" } },
        (res) => {
          const status = res.statusCode ?? 0;
          if (status >= 300 && status < 400 && res.headers.location) {
            res.resume();
            get(res.headers.location, redirects + 1);
            return;
          }
          if (status !== 200) {
            res.resume();
            fail(new Error(`HTTP ${status}`));
            return;
          }
          res.pipe(file);
          file.on("finish", () =>
            file.close((err) => {
              if (err) {
                fail(err);
                return;
              }
              try {
                fs.renameSync(part, dest);
                resolve();
              } catch (e) {
                fail(e as Error);
              }
            }),
          );
        },
      );
      req.on("error", fail);
      // Abort a stalled or half-open socket instead of hanging the
      // "Downloading…" notification forever; destroy(err) surfaces through the
      // error handler above. Each redirect is a fresh request, so set it here.
      req.setTimeout(30_000, () => req.destroy(new Error("download timed out")));
    };
    get(url, 0);
  });
}

/// Resolve the server binary: download the release matching this extension's
/// version into global storage (cached), so users don't need `cargo install`.
async function ensureServer(
  context: ExtensionContext,
): Promise<string | undefined> {
  const triple = targetTriple();
  if (!triple) {
    window.showErrorMessage(
      `solidity: no prebuilt server for ${process.platform}/${process.arch}. ` +
        `Set "solidity.serverPath" to a locally built solidity-for-foundry-lsp.`,
    );
    return undefined;
  }
  const version = context.extension.packageJSON.version as string;
  const exe = process.platform === "win32" ? ".exe" : "";
  const dir = context.globalStorageUri.fsPath;
  fs.mkdirSync(dir, { recursive: true });
  const dest = path.join(dir, `solidity-for-foundry-lsp-${version}${exe}`);
  if (fs.existsSync(dest)) {
    return dest;
  }
  const url = `https://github.com/${REPO}/releases/download/v${version}/solidity-for-foundry-lsp-${triple}${exe}`;
  try {
    await window.withProgress(
      { location: ProgressLocation.Notification, title: `Downloading solidity-for-foundry-lsp ${version}…` },
      () => download(url, dest),
    );
    if (process.platform !== "win32") {
      fs.chmodSync(dest, 0o755);
    }
    return dest;
  } catch (e) {
    fs.rmSync(dest, { force: true });
    window.showErrorMessage(
      `solidity: could not download the server (${e}). Set "solidity.serverPath" ` +
        "to a locally built solidity-for-foundry-lsp, or run `cargo install --path solidity-lsp`.",
    );
    return undefined;
  }
}

function newClient(command: string): LanguageClient {
  const run: Executable = { command };
  const serverOptions: ServerOptions = { run, debug: run };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "solidity" }],
    synchronize: {
      // .sol sources plus the project config: editing remappings or the solc
      // pin should re-resolve imports and re-check open files.
      fileEvents: workspace.createFileSystemWatcher(
        "**/{*.sol,foundry.toml,remappings.txt}",
      ),
    },
  };
  return new LanguageClient(
    "solidity",
    "Solidity for Foundry",
    serverOptions,
    clientOptions,
  );
}

/// Resolve the server command (an explicit solidity.serverPath, else the
/// downloaded release binary), then create and start the client. Recreating the
/// client each time — rather than client.restart() — is what lets an edited
/// serverPath take effect, since ServerOptions captures the command at
/// construction.
async function startClient(context: ExtensionContext): Promise<void> {
  const configured = workspace
    .getConfiguration("solidity")
    .get<string>("serverPath")
    ?.trim();
  const command =
    configured && configured.length > 0
      ? configured
      : await ensureServer(context);
  if (!command) {
    return;
  }
  client = newClient(command);
  await client.start();
}

export async function activate(context: ExtensionContext): Promise<void> {
  context.subscriptions.push(
    commands.registerCommand("solidity.restartServer", async () => {
      await client?.stop();
      await startClient(context);
    }),
    workspace.onDidChangeConfiguration(async (e) => {
      if (!e.affectsConfiguration("solidity.serverPath")) {
        return;
      }
      const pick = await window.showInformationMessage(
        "solidity.serverPath changed — restart the Solidity server to apply it.",
        "Restart",
      );
      if (pick === "Restart") {
        await commands.executeCommand("solidity.restartServer");
      }
    }),
  );
  await startClient(context);
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
