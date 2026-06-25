import {
  workspace,
  window,
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

const REPO = "u-zzam/solidity";

/// The release asset's Rust target triple for the current platform.
function targetTriple(): string | undefined {
  const key = `${process.platform}-${process.arch}`;
  return {
    "darwin-arm64": "aarch64-apple-darwin",
    "darwin-x64": "x86_64-apple-darwin",
    "linux-x64": "x86_64-unknown-linux-gnu",
    "win32-x64": "x86_64-pc-windows-msvc",
  }[key];
}

/// Download `url` to `dest`, following GitHub's redirect to the asset storage.
function download(url: string, dest: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest);
    const fail = (e: Error) => {
      file.destroy();
      fs.rm(dest, () => reject(e));
    };
    file.on("error", fail); // disk write failure (full disk, permissions)
    const get = (u: string, redirects: number) => {
      if (redirects > 5) {
        fail(new Error("too many redirects"));
        return;
      }
      https
        .get(u, { headers: { "User-Agent": "solidity-vscode" } }, (res) => {
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
          file.on("finish", () => file.close(() => resolve()));
        })
        .on("error", fail);
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
        `Set "solidity.serverPath" to a locally built solidity-lsp.`,
    );
    return undefined;
  }
  const version = context.extension.packageJSON.version as string;
  const exe = process.platform === "win32" ? ".exe" : "";
  const dir = context.globalStorageUri.fsPath;
  fs.mkdirSync(dir, { recursive: true });
  const dest = path.join(dir, `solidity-lsp-${version}${exe}`);
  if (fs.existsSync(dest)) {
    return dest;
  }
  const url = `https://github.com/${REPO}/releases/download/v${version}/solidity-lsp-${triple}${exe}`;
  try {
    await window.withProgress(
      { location: ProgressLocation.Notification, title: `Downloading solidity-lsp ${version}…` },
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
        "to a locally built solidity-lsp, or run `cargo install --path solidity-lsp`.",
    );
    return undefined;
  }
}

export async function activate(context: ExtensionContext): Promise<void> {
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
