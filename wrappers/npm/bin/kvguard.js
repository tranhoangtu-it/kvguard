#!/usr/bin/env node
// Runs the native kvguard binary, downloading it first when postinstall was skipped
// (e.g. `npm install --ignore-scripts`).
"use strict";

const fs = require("node:fs");
const { spawnSync } = require("node:child_process");
const { install, BINARY } = require("../install.js");

async function main() {
  if (!fs.existsSync(BINARY)) {
    await install();
  }
  const result = spawnSync(BINARY, process.argv.slice(2), { stdio: "inherit" });
  if (result.error) throw result.error;
  process.exit(result.status ?? 1);
}

main().catch((err) => {
  console.error(`kvguard: ${err.message}`);
  process.exit(1);
});
