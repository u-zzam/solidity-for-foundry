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
import * as stream from "stream";
import * as crypto from "crypto";

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
    // that the next launch trusts via existsSync. The pid keeps two windows'
    // first-run downloads from clobbering each other's temp file.
    const part = `${dest}.part-${process.pid}`;
    const file = fs.createWriteStream(part);
    let settled = false;
    const fail = (e: Error) => {
      if (settled) return;
      settled = true;
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
          // A proxy/CDN that FINs mid-body closes the socket cleanly — no
          // `error`, no `timeout` (destroy clears the inactivity timer) — which
          // would hang the "Downloading…" promise forever. Surface it both via
          // res.complete and pipeline's ERR_STREAM_PREMATURE_CLOSE.
          res.on("close", () => {
            if (!res.complete) {
              fail(new Error("connection closed before download completed"));
            }
          });
          stream.pipeline(res, file, (err) => {
            if (err) {
              fail(err);
              return;
            }
            try {
              // chmod the .part before the atomic rename so `dest` is never
              // observed non-executable (a kill between rename and chmod used to
              // leave a permanently unspawnable cached binary).
              if (process.platform !== "win32") {
                fs.chmodSync(part, 0o755);
              }
              fs.renameSync(part, dest);
              settled = true;
              resolve();
            } catch (e) {
              fail(e as Error);
            }
          });
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

/// GET `url` as text, following redirects. Resolves `undefined` on 404 (a
/// missing sibling asset), rejects on any other non-200.
function fetchText(url: string): Promise<string | undefined> {
  return new Promise((resolve, reject) => {
    const get = (u: string, redirects: number) => {
      if (redirects > 5) {
        reject(new Error("too many redirects"));
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
          if (status === 404) {
            res.resume();
            resolve(undefined);
            return;
          }
          if (status !== 200) {
            res.resume();
            reject(new Error(`HTTP ${status}`));
            return;
          }
          let body = "";
          res.setEncoding("utf8");
          res.on("data", (c) => (body += c));
          res.on("close", () => {
            if (!res.complete) reject(new Error("connection closed"));
          });
          res.on("end", () => resolve(body));
        },
      );
      req.on("error", reject);
      req.setTimeout(30_000, () => req.destroy(new Error("checksum timed out")));
    };
    get(url, 0);
  });
}

/// Verify the downloaded binary against its sibling `<asset>.sha256`. Deletes
/// `dest` and throws on mismatch; skips silently when the checksum asset 404s
/// (older releases published no checksums).
async function verifyChecksum(dest: string, assetUrl: string): Promise<void> {
  const sums = await fetchText(`${assetUrl}.sha256`);
  if (sums === undefined) {
    return;
  }
  const expected = sums.trim().split(/\s+/)[0]?.toLowerCase();
  const actual = crypto
    .createHash("sha256")
    .update(fs.readFileSync(dest))
    .digest("hex");
  if (expected !== actual) {
    fs.rmSync(dest, { force: true });
    throw new Error("server binary failed checksum verification");
  }
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
    await verifyChecksum(dest, url);
    // Reclaim disk from prior-version binaries (16–21MB each, never reused).
    const current = path.basename(dest);
    for (const name of fs.readdirSync(dir)) {
      if (
        name.startsWith("solidity-for-foundry-lsp-") &&
        !name.includes(".part") &&
        name !== current
      ) {
        fs.rmSync(path.join(dir, name), { force: true });
      }
    }
    return dest;
  } catch (e) {
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
    initializationOptions: {
      experimental: {
        inlayHints: workspace
          .getConfiguration("solidity")
          .get<boolean>("experimental.inlayHints", true),
      },
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
  if (configured && configured.length > 0) {
    client = newClient(configured);
    await client.start();
    return;
  }
  // Downloaded binary: a cache that spawn-fails (killed mid-write, a 200 HTML
  // error page slipped past, a partial chmod) fails every launch. Delete it and
  // re-download once before giving up.
  for (let attempt = 0; attempt < 2; attempt++) {
    const command = await ensureServer(context);
    if (!command) {
      return;
    }
    client = newClient(command);
    try {
      await client.start();
      return;
    } catch (e) {
      await client.stop().catch(() => {});
      if (attempt === 0) {
        fs.rmSync(command, { force: true });
        continue;
      }
      window.showErrorMessage(
        `solidity: the server binary failed to start (${e}). ` +
          'Set "solidity.serverPath" to a locally built solidity-for-foundry-lsp.',
      );
    }
  }
}

export async function activate(context: ExtensionContext): Promise<void> {
  context.subscriptions.push(
    commands.registerCommand("solidity.restartServer", async () => {
      // stop() throws when the client is in StartFailed state — the exact case
      // this restart is meant to recover from — so swallow it.
      await client?.stop().catch(() => {});
      await startClient(context);
    }),
    workspace.onDidChangeConfiguration(async (e) => {
      if (
        !e.affectsConfiguration("solidity.serverPath") &&
        !e.affectsConfiguration("solidity.experimental.inlayHints")
      ) {
        return;
      }
      const pick = await window.showInformationMessage(
        "Solidity setting changed — restart the server to apply it.",
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
  return client?.stop().catch(() => {});
}
