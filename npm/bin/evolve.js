#!/usr/bin/env node
"use strict";
const { spawnSync } = require("child_process");
const { install } = require("../install.js");

install()
  .then((exe) => {
    const r = spawnSync(exe, process.argv.slice(2), { stdio: "inherit" });
    if (r.error) throw r.error;
    process.exit(r.status === null ? 1 : r.status);
  })
  .catch((err) => {
    process.stderr.write(`evolve-os: ${err.message}\n`);
    process.exit(1);
  });
