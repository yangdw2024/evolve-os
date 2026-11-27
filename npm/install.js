#!/usr/bin/env node
"use strict";
// Downloads the prebuilt `evolve` binary for this platform from GitHub Releases.
// Set EVOLVE_OS_SKIP_DOWNLOAD=1 to skip (e.g. offline CI); the binary is fetched lazily on first run.
const fs = require("fs");
const os = require("os");
const path = require("path");
const https = require("https");
const { execFileSync } = require("child_process");

const pkg = require("./package.json");
// Release binaries are versioned independently so npm-only fixes don't need a new GitHub release.
const VERSION = pkg.evolveOsBinaryVersion || pkg.version;
const REPO = "yangdw2024/evolve-os";

function target() {
  const key = `${process.platform}-${process.arch}`;
  const map = {
    "win32-x64": "x86_64-pc-windows-msvc",
    "linux-x64": "x86_64-unknown-linux-gnu",
    "darwin-x64": "x86_64-apple-darwin",
    "darwin-arm64": "aarch64-apple-darwin",
  };
  if (!map[key]) throw new Error(`evolve-os: unsupported platform ${key}; build from source with \`cargo install evolve-os\``);
  return map[key];
}

function binPath() {
  return path.join(__dirname, "vendor", process.platform === "win32" ? "evolve.exe" : "evolve");
}

function download(url, dest, redirects = 5) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": "evolve-os-npm" } }, (res) => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location && redirects > 0) {
          res.resume();
          return resolve(download(res.headers.location, dest, redirects - 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`download ${url} failed: HTTP ${res.statusCode}`));
        }
        const out = fs.createWriteStream(dest);
        res.pipe(out);
        out.on("finish", () => out.close(resolve));
        out.on("error", reject);
      })
      .on("error", reject);
  });
}

function extract(file, dir) {
  if (process.platform !== "win32") {
    execFileSync("tar", ["-xzf", file, "-C", dir], { stdio: "inherit" });
    return;
  }
  // A GNU tar from Git/MSYS/conda on PATH misreads `C:\...` as a remote host, so use the
  // Windows-bundled bsdtar explicitly, falling back to PowerShell Expand-Archive.
  const sysTar = path.join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe");
  if (fs.existsSync(sysTar)) {
    try {
      execFileSync(sysTar, ["-xf", file, "-C", dir], { stdio: "inherit" });
      return;
    } catch (_) {
      /* fall through */
    }
  }
  execFileSync(
    "powershell.exe",
    ["-NoProfile", "-NonInteractive", "-Command", `Expand-Archive -LiteralPath '${file}' -DestinationPath '${dir}' -Force`],
    { stdio: "inherit" }
  );
}

async function install() {
  const dest = binPath();
  if (fs.existsSync(dest)) return dest;
  const t = target();
  const tag = `v${VERSION}`;
  const name = `evolve-${tag}-${t}`;
  const archive = process.platform === "win32" ? `${name}.zip` : `${name}.tar.gz`;
  const url = `https://github.com/${REPO}/releases/download/${tag}/${archive}`;
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "evolve-os-"));
  const file = path.join(tmp, archive);
  process.stderr.write(`evolve-os: downloading ${url}\n`);
  await download(url, file);
  extract(file, tmp);
  const exe = path.join(tmp, name, process.platform === "win32" ? "evolve.exe" : "evolve");
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.copyFileSync(exe, dest);
  if (process.platform !== "win32") fs.chmodSync(dest, 0o755);
  const examples = path.join(tmp, name, "examples");
  if (fs.existsSync(examples)) fs.cpSync(examples, path.join(__dirname, "vendor", "examples"), { recursive: true });
  return dest;
}

module.exports = { install, binPath };

if (require.main === module) {
  if (process.env.EVOLVE_OS_SKIP_DOWNLOAD === "1") process.exit(0);
  install().catch((err) => {
    // Do not fail `npm install`; the launcher retries on first use.
    process.stderr.write(`evolve-os: postinstall download failed (${err.message}); will retry on first run\n`);
  });
}
