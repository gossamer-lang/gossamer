// Runs Gossamer programs through the browser playground's wasm build and
// reports any that end the module rather than answering.
//
// wasm32-unknown-unknown has no unwinder, so a Rust panic anywhere under
// `run` aborts the module: the call throws `RuntimeError: unreachable`
// and the program gets no output at all. Every other outcome - a
// front-end rejection, a runtime error, an exit status - comes back
// inside the result, so a throw is the whole failure condition here. The
// files after `--must-run` are programs the site runs in the browser, and
// each of those must also answer without an error.
//
// Usage: node scripts/playground_smoke.js <bindgen-dir> <file.gos>...
//        [--must-run <file.gos>...]
"use strict";

const fs = require("fs");
const path = require("path");

const [bindgenDir, ...args] = process.argv.slice(2);
const split = args.indexOf("--must-run");
const files = split < 0 ? args : [...args.slice(0, split), ...args.slice(split + 1)];
const mustRun = new Set(split < 0 ? [] : args.slice(split + 1));
if (!bindgenDir || files.length === 0) {
  console.error("usage: node playground_smoke.js <bindgen-dir> <file.gos>...");
  process.exit(2);
}

const pg = require(path.resolve(bindgenDir, "gossamer_playground.js"));

let trapped = 0;
let failed = 0;
for (const file of files) {
  const source = fs.readFileSync(file, "utf8");
  try {
    const result = pg.run(source, undefined);
    if (mustRun.has(file) && result && result.error) {
      failed += 1;
      console.log("FAIL " + file + ": " + String(result.error).split("\n")[0]);
    } else {
      console.log("ok   " + file);
    }
  } catch (err) {
    trapped += 1;
    const panic = typeof pg.last_panic === "function" ? pg.last_panic() : "";
    const reason = panic || (err && err.message) || String(err);
    console.log("TRAP " + file + ": " + reason.split("\n")[0]);
  }
}
console.log(`${files.length} program(s), ${trapped} trapped, ${failed} site program(s) failed`);
process.exit(trapped === 0 && failed === 0 ? 0 : 1);
