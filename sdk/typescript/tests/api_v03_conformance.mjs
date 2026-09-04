import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import * as api from "../src/api_v03.mjs";

const fixtures = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../protocol/fixtures/extension-api-v0.3",
);
const negativeFixtures = resolve(fixtures, "negative");
const readFixture = (name) => JSON.parse(readFileSync(resolve(fixtures, `${name}.json`), "utf8"));
const errorCode = (operation) => {
  assert.throws(operation, (error) => error instanceof api.ContractError);
  try {
    operation();
  } catch (error) {
    return error.code;
  }
  throw new Error("expected generated contract error");
};

const manifest = readFixture("manifest");
assert.equal(manifest.api_version, api.API_VERSION);
assert.equal(manifest.canonical_encoding, api.CANONICAL_ENCODING);
for (const entry of manifest.fixtures) {
  const raw = readFileSync(resolve(fixtures, `${entry.name}.json`), "utf8");
  assert.equal(api.canonicalJson(JSON.parse(raw)), raw);
}

api.validateInitializeRequest(api.parseInitializeRequest(readFixture("initialize-request")));
api.validateInitializeResponse(api.parseInitializeResponse(readFixture("initialize-response")));
api.validateToolCallParams(api.parseToolCallParams(readFixture("tool-call-params")));
const toolResult = api.parseToolCallResult(readFixture("tool-call-result"));
api.validateToolCallResult(toolResult);
assert.equal(toolResult.structured_content.kind, "value");
assert.deepEqual(toolResult.structured_content.value, { value: "hello" });
api.validateCancelRequestParams(api.parseCancelRequestParams(readFixture("cancel-request-params")));
api.validateShutdownParams(api.parseShutdownParams(readFixture("shutdown-params")));
api.validateShutdownResult(api.parseShutdownResult(readFixture("shutdown-result")));
api.validateErrorObject(api.parseErrorObject(readFixture("error-data-absent")));
api.validateErrorObject(api.parseErrorObject(readFixture("error-data-null")));
api.validateDisposition(api.parseDisposition(readFixture("continue-disposition")));
for (const name of ["request-envelope", "notification-envelope", "success-envelope", "error-envelope"]) {
  api.parseJsonRpcEnvelope(readFixture(name));
}

const absent = api.parseErrorObject(readFixture("error-data-absent"));
const explicitNull = api.parseErrorObject(readFixture("error-data-null"));
assert.equal(absent.data.kind, "absent");
assert.equal(explicitNull.data.kind, "null");
assert.equal(errorCode(() => api.parseDisposition({ kind: "continue", reason: null })), -32602);
assert.equal(
  errorCode(() => api.parseToolCallResult({
    content: [{ type: "image", artifact_id: "a", mime_type: "image/png" }],
    is_error: false,
    metadata: null,
  })),
  -32011,
);

const offer = api.hostOffer(api.MAX_FRAME_BYTES * 2, api.MAX_CONCURRENT_REQUESTS * 2);
assert.equal(offer.limits.max_frame_bytes, api.MAX_FRAME_BYTES);
assert.equal(offer.limits.max_concurrent_requests, api.MAX_CONCURRENT_REQUESTS);
const negotiated = api.negotiate(offer, api.selectRequired(offer));
api.requireMethod(negotiated, "initialize", "host_to_extension");
assert.equal(errorCode(() => api.requireMethod(negotiated, "future/call", "host_to_extension")), -32601);
assert.equal(errorCode(() => api.requireMethod(negotiated, "context/collect", "host_to_extension")), -32601);
assert.equal(errorCode(() => api.validateOffer({ ...offer, required_methods: ["initialize"] })), -32011);
assert.equal(errorCode(() => api.validateInitializeRequest({ ...readFixture("initialize-request"), api_version: "0.2" })), -32602);
assert.equal(errorCode(() => api.validateErrorObject({ code: -32601, message: "incorrect error message" })), -32602);
assert.equal(errorCode(() => api.canonicalJson({ float: 1.5 })), -32602);
assert.equal(errorCode(() => api.canonicalJson({ large: api.MAX_PORTABLE_JSON_INTEGER + 1 })), -32602);
assert.equal(errorCode(() => api.canonicalFrame({ x: "y" }, 1)), -32012);
assert.equal(errorCode(() => api.parseJsonRpcEnvelope({ jsonrpc: "2.0", id: null, result: {} })), -32600);
assert.equal(errorCode(() => api.parseJsonRpcEnvelope({ jsonrpc: "2.0", id: 1, result: {}, error: { code: -32600, message: "invalid request" } })), -32600);
assert.equal(errorCode(() => api.canonicalJson({ "\ud800": "bad" })), -32602);

for (const entry of JSON.parse(readFileSync(resolve(negativeFixtures, "manifest.json"), "utf8")).fixtures) {
  const raw = readFileSync(resolve(negativeFixtures, `${entry.name}.json`), "utf8");
  if (entry.name === "duplicate-key") {
    const parsed = JSON.parse(raw);
    assert.notEqual(api.canonicalJson(parsed), raw);
  } else if (entry.name.includes("surrogate")) {
    assert.equal(errorCode(() => api.canonicalJson(JSON.parse(raw))), -32602);
  } else if (entry.name === "optional-reason-null") {
    assert.equal(errorCode(() => api.parseDisposition(JSON.parse(raw))), -32602);
  } else {
    assert.equal(errorCode(() => api.parseJsonRpcEnvelope(JSON.parse(raw))), -32600);
  }
}

assert.equal(api.runtimeSupportsApiVersion("0.1"), true);
assert.equal(api.bundleSupportsApiVersion("0.1"), false);
assert.equal(api.bundleSupportsApiVersion("0.2"), true);
assert.equal(api.bundleSupportsApiVersion("0.3"), true);
assert.deepEqual(api.LEGACY_ADAPTERS[0], { version: "0.1", status: "frozen", wire: "legacy-json-rpc" });
assert.deepEqual(api.LEGACY_ADAPTERS[1], { version: "0.2", status: "supported", wire: "legacy-json-rpc" });

console.log(`TypeScript API 0.3 conformance: ${manifest.fixtures.length} fixtures`);
