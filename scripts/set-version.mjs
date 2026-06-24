#!/usr/bin/env node
// Writes a SemVer version (from the git tag) into tauri.conf.json + Cargo.toml so the
// embedded binary version, the .dmg name, and latest.json never drift.
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

export function setVersion(version) {
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(`not a valid SemVer: ${version}`);
  }
  // tauri.conf.json
  const confPath = join(root, "src-tauri", "tauri.conf.json");
  const conf = JSON.parse(readFileSync(confPath, "utf8"));
  conf.version = version;
  writeFileSync(confPath, JSON.stringify(conf, null, 2) + "\n");
  // Cargo.toml — replace the version on the first occurrence in [package]
  const cargoPath = join(root, "src-tauri", "Cargo.toml");
  const cargo = readFileSync(cargoPath, "utf8");
  const updated = cargo.replace(/^version = "[^"]*"/m, `version = "${version}"`);
  writeFileSync(cargoPath, updated);
  return version;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const raw = process.argv[2] || "";
  const version = raw.replace(/^v/, ""); // strip leading "v" from tags like v1.4.0
  setVersion(version);
  console.log(`set version to ${version}`);
}
