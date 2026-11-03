#!/usr/bin/env node
// Downloads the kvguard binary for this platform from GitHub Releases, verifies its SHA-256
// checksum and unpacks it into ./native. Uses only Node built-ins and the system `tar`.
"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const https = require("node:https");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const REPO = "tranhoangtu-it/kvguard";
const TARGETS = {
  "linux-x64": "x86_64-unknown-linux-musl",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
  "win32-x64": "x86_64-pc-windows-msvc",
};
const MAX_REDIRECTS = 5;

const NATIVE_DIR = path.join(__dirname, "native");
const BINARY = path.join(NATIVE_DIR, process.platform === "win32" ? "kvguard.exe" : "kvguard");

function assetName(version) {
  const target = TARGETS[`${process.platform}-${process.arch}`];
  if (!target) {
    throw new Error(`kvguard has no prebuilt binary for ${process.platform}-${process.arch}`);
  }
  const ext = process.platform === "win32" ? "zip" : "tar.gz";
  return `kvguard-v${version}-${target}.${ext}`;
}

function download(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": "kvguard-npm-installer" } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          if (redirects >= MAX_REDIRECTS) return reject(new Error(`too many redirects for ${url}`));
          return resolve(download(new URL(res.headers.location, url).toString(), redirects + 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`GET ${url} failed with HTTP ${res.statusCode}`));
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

async function install() {
  const { version } = require("./package.json");
  const asset = assetName(version);
  const base = `https://github.com/${REPO}/releases/download/v${version}`;
  const [archive, checksumFile] = await Promise.all([
    download(`${base}/${asset}`),
    download(`${base}/${asset}.sha256`),
  ]);

  const expected = checksumFile.toString("utf8").trim().split(/\s+/)[0].toLowerCase();
  const actual = crypto.createHash("sha256").update(archive).digest("hex");
  if (expected !== actual) {
    throw new Error(`checksum mismatch for ${asset}: expected ${expected}, got ${actual}`);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "kvguard-"));
  try {
    const archivePath = path.join(tmp, asset);
    fs.writeFileSync(archivePath, archive);
    fs.mkdirSync(NATIVE_DIR, { recursive: true });
    // bsdtar (Windows 10+, macOS) and GNU tar both read .tar.gz; bsdtar also reads .zip.
    execFileSync("tar", ["-xf", archivePath, "-C", NATIVE_DIR], { stdio: "inherit" });
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
  if (process.platform !== "win32") fs.chmodSync(BINARY, 0o755);
  return BINARY;
}

module.exports = { install, BINARY };

if (require.main === module) {
  install()
    .then((bin) => console.log(`kvguard installed at ${bin}`))
    .catch((err) => {
      console.error(`kvguard: install failed: ${err.message}`);
      process.exit(1);
    });
}
