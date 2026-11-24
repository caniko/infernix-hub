import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";

const hooks = registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "windmill-client") {
      return {
        url: "data:text/javascript,export const getVariable = (path) => globalThis.readWindmillVariable(path)",
        shortCircuit: true,
      };
    }
    return nextResolve(specifier, context);
  },
});
const { main } = await import("../registry/windmill/humanize.ts");
hooks.deregister();

for (const [base, token, host, bearer] of [
  [undefined, undefined, "127.0.0.1:3000", null],
  ["http://localhost:3000", "   ", "localhost:3000", null],
  ["http://[::1]:3000", undefined, "[::1]:3000", null],
  ["https://humanizer.example", " test-token ", "humanizer.example", "Bearer test-token"],
  ["https://humanizer.example", undefined, null, null],
  ["http://localhost.example:3000", " ", null, null],
]) {
  test(`${base ?? "default URL"} with ${bearer ? "a" : "no"} token`, async (t) => {
    t.mock.method(globalThis, "fetch", async () =>
      Response.json({ humanized_text: "Hi @fan", judge_verified: true, fallback_paragraphs: [] })
    );
    globalThis.readWindmillVariable = async (path) => {
      const value = path === "f/fleetix_hub/humanizer_token" ? token : base;
      if (value === undefined) throw new Error("variable not found");
      return value;
    };
    try {
      const result = await main("Hello @fan", ["@fan"], 20, 2);
      assert.equal(globalThis.fetch.mock.callCount(), host ? 1 : 0);
      if (!host) {
        assert.equal(result.humanized_text, "Hello @fan");
        assert.equal(result.reason, "humanizer token is required for non-loopback URLs");
        return;
      }
      const [url, request] = globalThis.fetch.mock.calls[0].arguments;
      assert.equal(url.host, host);
      assert.equal(url.pathname, "/api/v1/humanize");
      assert.equal(new Headers(request.headers).get("authorization"), bearer);
      assert.equal(request.redirect, "error");
      assert.deepEqual(JSON.parse(request.body), { text: "Hello @fan", locked_facts: ["@fan"], max_chars: 20, best_of_n: 2 });
      assert.equal(result.applied, true);
    } finally {
      delete globalThis.readWindmillVariable;
    }
  });
}
