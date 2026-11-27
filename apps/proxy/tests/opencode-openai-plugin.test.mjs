import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { install, validateOptions, assertProxyDestination } from "../opencode-openai-plugin/server.js";

test("validateOptions rejects non-loopback URLs, wrong paths, and store key paths", () => {
  assert.throws(() => validateOptions({ baseURL: "https://api.openai.com/backend-api/codex", clientKeyFile: "/run/key" }));
  assert.throws(() => validateOptions({ baseURL: "http://127.0.0.1:8317/v1", clientKeyFile: "/run/key" }));
  assert.throws(() => validateOptions({ baseURL: "http://127.0.0.1:8317/backend-api/codex", clientKeyFile: "/nix/store/abc-key" }));
  const ok = validateOptions({ baseURL: "http://127.0.0.1:8317/backend-api/codex/", clientKeyFile: "/run/agenix/key" });
  assert.equal(ok.baseURL, "http://127.0.0.1:8317/backend-api/codex");
  assert.equal(ok.expectedOrigin, "http://127.0.0.1:8317");
  assert.equal(ok.expectedPrefix, "/backend-api/codex/");
});

test("assertProxyDestination blocks cross-origin and unexpected paths", () => {
  assertProxyDestination("http://127.0.0.1:8317/backend-api/codex/responses", "http://127.0.0.1:8317", "/backend-api/codex/");
  assert.throws(() =>
    assertProxyDestination("https://chatgpt.com/backend-api/codex/responses", "http://127.0.0.1:8317", "/backend-api/codex/"),
  );
  assert.throws(() =>
    assertProxyDestination("http://127.0.0.1:8317/v8/management/credentials", "http://127.0.0.1:8317", "/backend-api/codex/"),
  );
});

test("install detaches openai integrationID and injects runtime client key on http.request", async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), "fleetix-openai-plugin-"));
  const keyFile = path.join(dir, "client-key");
  await writeFile(keyFile, "fixture-client-key-v1\n", { mode: 0o600 });
  try {
    const provider = {
      id: "openai",
      activation: "auto",
      integrationID: "openai",
      settings: { baseURL: "https://api.openai.com/v1", transport: "websocket" },
      headers: {},
      body: {},
    };
    const model = {
      id: "gpt-6.2-sol",
      providerID: "openai",
      cost: [{ input: 5, output: 15 }],
      body: {},
    };
    const hooks = new Map();
    const ctx = {
      provider: {
        async transform(fn) {
          fn({
            get(id) {
              return id === "openai" ? { provider, models: new Map([[model.id, model]]) } : undefined;
            },
            update(id, updater) {
              assert.equal(id, "openai");
              updater(provider);
            },
          });
        },
      },
      model: {
        async transform(fn) {
          fn({
            list: () => [model],
            update(providerID, id, updater) {
              assert.equal(providerID, "openai");
              assert.equal(id, model.id);
              updater(model);
            },
          });
        },
      },
      session: {
        async hook(name, fn, filter) {
          assert.equal(filter.providerID, "openai");
          hooks.set(name, fn);
        },
      },
    };

    await install(ctx, { baseURL: "http://127.0.0.1:8317/backend-api/codex", clientKeyFile: keyFile });

    assert.equal(provider.activation, "enabled");
    assert.equal(provider.integrationID, "fleetix-agent-proxy-openai");
    assert.equal(provider.settings.baseURL, "http://127.0.0.1:8317/backend-api/codex");
    assert.equal(provider.settings.transport, "http");
    assert.equal(provider.body.store, false);
    assert.deepEqual(model.cost, []);
    assert.equal(model.body.store, false);

    const modelReq = { baseURL: "https://api.openai.com/v1", headers: {}, body: {}, sessionID: "sess-123" };
    hooks.get("model.request")(modelReq);
    assert.equal(modelReq.baseURL, "http://127.0.0.1:8317/backend-api/codex");
    assert.equal(modelReq.headers["session-id"], "sess-123");
    assert.equal(modelReq.headers["x-session-id"], "sess-123");
    assert.equal(modelReq.headers.originator, "opencode");
    assert.equal(modelReq.headers["x-codex-beta-features"], "remote_compaction_v2");
    assert.equal(modelReq.body.store, false);

    const httpEvent = {
      request: new Request("http://127.0.0.1:8317/backend-api/codex/responses", {
        method: "POST",
        headers: {
          authorization: "Bearer stale-token",
          "chatgpt-account-id": "stale-account",
        },
      }),
    };
    await hooks.get("http.request")(httpEvent);
    assert.equal(httpEvent.request.headers.get("authorization"), "Bearer fixture-client-key-v1");
    assert.equal(httpEvent.request.headers.get("chatgpt-account-id"), null);

    // Key rotation on disk takes effect without restarting OpenCode.
    await writeFile(keyFile, "fixture-client-key-v2\n", { mode: 0o600 });
    await hooks.get("http.request")(httpEvent);
    assert.equal(httpEvent.request.headers.get("authorization"), "Bearer fixture-client-key-v2");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
