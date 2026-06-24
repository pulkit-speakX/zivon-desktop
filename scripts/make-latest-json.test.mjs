import { test } from "node:test";
import assert from "node:assert/strict";
import { buildLatestJson } from "./make-latest-json.mjs";

test("emits both darwin arch keys pointing at the same artifact", () => {
  const out = buildLatestJson({
    version: "1.4.0",
    notes: "Bug fixes.",
    pubDate: "2026-06-24T10:30:00Z",
    signature: "SIG_CONTENT",
    url: "https://downloads.zivon.ai/Envoy_1.4.0.app.tar.gz",
  });
  assert.equal(out.version, "1.4.0");
  assert.equal(out.notes, "Bug fixes.");
  assert.equal(out.pub_date, "2026-06-24T10:30:00Z");
  const aarch = out.platforms["darwin-aarch64"];
  const intel = out.platforms["darwin-x86_64"];
  assert.equal(aarch.signature, "SIG_CONTENT");
  assert.equal(intel.signature, "SIG_CONTENT");
  assert.equal(aarch.url, "https://downloads.zivon.ai/Envoy_1.4.0.app.tar.gz");
  assert.equal(intel.url, aarch.url);
});

test("throws when signature is empty", () => {
  assert.throws(() =>
    buildLatestJson({ version: "1.0.0", notes: "", pubDate: "x", signature: "", url: "u" }),
  );
});
