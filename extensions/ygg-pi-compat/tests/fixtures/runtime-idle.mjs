#!/usr/bin/env node
// Hermetic no-extension baseline for bench-pi-runtime.py. It deliberately
// implements only readiness, activation, and shutdown on the same NDJSON shape.

import { createInterface } from "node:readline";

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", (line) => {
  if (!line.trim()) return;
  const message = JSON.parse(line);
  if (message.id === undefined) return;
  if (message.method === "initialize") {
    process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: message.id, result: { api_version: "baseline" } })}\n`);
  } else if (message.method === "activate") {
    process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: message.id, result: { ok: true } })}\n`);
  } else if (message.method === "shutdown") {
    process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: message.id, result: {} })}\n`);
    setImmediate(() => process.exit(0));
  } else {
    process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: message.id, error: { code: -32601, message: "unknown baseline method" } })}\n`);
  }
});
