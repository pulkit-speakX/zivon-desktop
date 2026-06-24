#!/usr/bin/env node
// Generates the Tauri updater manifest (latest.json) for the macOS universal build.
// A universal .app reports darwin-aarch64 OR darwin-x86_64 at runtime (never
// "darwin-universal"), so both keys point at the same artifact + signature.
import { readFileSync, writeFileSync } from "node:fs";

export function buildLatestJson({ version, notes, pubDate, signature, url }) {
  if (!version) throw new Error("version is required");
  if (!signature) throw new Error("signature is required");
  if (!url) throw new Error("url is required");
  const platform = { signature, url };
  return {
    version,
    notes: notes ?? "",
    pub_date: pubDate,
    platforms: {
      "darwin-aarch64": platform,
      "darwin-x86_64": platform,
    },
  };
}

// CLI: node make-latest-json.mjs <version> <sigFile> <url> [notes] > latest.json
if (import.meta.url === `file://${process.argv[1]}`) {
  const [, , version, sigFile, url, notes = ""] = process.argv;
  if (!version || !sigFile || !url) {
    console.error("usage: make-latest-json.mjs <version> <sigFile> <url> [notes]");
    process.exit(1);
  }
  const signature = readFileSync(sigFile, "utf8").trim();
  const pubDate = new Date().toISOString();
  const manifest = buildLatestJson({ version, notes, pubDate, signature, url });
  const json = JSON.stringify(manifest, null, 2);
  if (process.env.LATEST_JSON_OUT) {
    writeFileSync(process.env.LATEST_JSON_OUT, json);
  } else {
    process.stdout.write(json + "\n");
  }
}
