// Evaluated from the supplied T3 checkout's apps/server directory, not from this proxy.
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import * as NodeCrypto from "@effect/platform-node/NodeCrypto";
import * as DateTime from "effect/DateTime";
import * as Effect from "effect/Effect";
import { HttpClient, HttpClientResponse } from "effect/http";
import * as TestClock from "effect/testing/TestClock";
import { limitRecoveryCommand } from "./src/orchestration-v2/UsageLimitRecoveryWorker.ts";
import { watchAntigravityPromptOutcome } from "./src/provider/acp/AntigravityTurnOutcome.ts";
import { makeCliproxyApi } from "./src/usage/cliproxyApi.ts";

const [profile, proxy, mock, completedAt, ...errors] = process.argv.slice(1);
const completedMs = Number(completedAt);
const conversations = join(profile, "antigravity-acp", "conversations");
mkdirSync(conversations, { recursive: true });
const [outcome] = await Promise.all(
  errors.map(async (error, index) => {
    const session = `verify-turn-${index}`;
    const classify = await Effect.runPromise(watchAntigravityPromptOutcome(profile, session));
    const db = new DatabaseSync(join(conversations, `${session}.db`));
    try {
      db.exec("CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, step_payload BLOB)");
      db.prepare("INSERT INTO steps VALUES (1, 17, ?)").run(Buffer.from(error));
    } finally {
      db.close();
    }
    const result = await Effect.runPromise(
      Effect.gen(function* () {
        yield* TestClock.setTime(completedMs);
        return yield* classify({ stopReason: "end_turn" });
      }).pipe(Effect.provide(TestClock.layer())),
    );
    const outcome = result?.failure ?? result;
    assert.equal(outcome.class, "usage_limit");
    assert.ok(Date.parse(outcome.resetAt) >= completedMs + 5000);
    return outcome;
  }),
);
const resetMs = Date.parse(outcome.resetAt);

const thread = {
  id: "thread:verify",
  status: "failed",
  lastErrorClass: outcome.class,
  usageLimitResetAt: outcome.resetAt,
  latestRunId: "run:verify",
  latestRunCompletedAt: DateTime.makeUnsafe(completedMs),
  updatedAt: DateTime.makeUnsafe(completedMs),
  archivedAt: null,
  settledOverride: null,
  pendingRuntimeRequest: null,
  limitRecovery: null,
  snoozedUntil: null,
};
assert.equal(
  limitRecoveryCommand({ ...thread, usageLimitResetAt: new Date(completedMs).toISOString() }, true, completedMs),
  null,
);
const arm = limitRecoveryCommand(thread, true, completedMs);
assert.equal(arm?.type, "thread.metadata.update");
const armed = { ...thread, limitRecovery: arm.limitRecovery };
assert.equal(limitRecoveryCommand(armed, true, resetMs - 1), null);
const dispatch = limitRecoveryCommand(armed, true, resetMs);
assert.equal(dispatch?.type, "message.dispatch");
assert.equal(dispatch.usageLimitContinuationOfRunId, thread.latestRunId);
assert.equal(limitRecoveryCommand(armed, true, resetMs + 1)?.commandId, dispatch.commandId);

// Forward real T3 management requests to the loopback proxy. Rewrite only the
// literal provider URLs below to the loopback upstream; no external calls.
const wham = "https://chatgpt.com/backend-api/wham";
const usage = `${wham}/usage`;
const credits = `${wham}/rate-limit-reset-credits`;
const claude = "https://api.anthropic.com/api/oauth/usage";
const probes = [];
const mutations = [];
const http = HttpClient.make((request, url) =>
  Effect.tryPromise(async () => {
    assert.equal(url.origin, proxy);
    let body;
    if (request.body._tag === "Uint8Array") {
      body = JSON.parse(new TextDecoder().decode(request.body.body));
    }
    if (url.pathname.endsWith("/api-call")) {
      const target = body.url;
      if (body.method === "GET") {
        assert.ok([usage, credits, claude].includes(target), `unexpected T3 usage probe ${target}`);
        probes.push(target);
      } else {
        assert.equal(body.method, "POST");
        assert.equal(target, `${credits}/consume`);
        mutations.push("consume");
      }
      body.url = mock + new URL(target).pathname;
    } else if (url.pathname.endsWith("/reset-quota")) {
      mutations.push("reset-quota");
    }
    const headers = new Headers(request.headers);
    headers.delete("content-length");
    const response = await fetch(url, {
      headers,
      method: request.method,
      body: body === undefined ? undefined : JSON.stringify(body),
      redirect: "error",
    });
    return HttpClientResponse.fromWeb(request, response);
  }),
);

await Effect.runPromise(
  Effect.gen(function* () {
    const api = yield* makeCliproxyApi;
    const hub = { url: proxy, managementKey: "verify-mgmt-key" };
    const accounts = yield* api.readAccounts(hub);
    assert.deepEqual(accounts.map((account) => account.driver).sort(), ["claudeAgent", "codex"]);
    assert.deepEqual(probes.sort(), [usage, credits, claude].sort());
    const codex = accounts.find((account) => account.driver === "codex");
    assert.equal(codex.usageLimits.resetCredits.availableCount, 1);
    const credit = codex.usageLimits.resetCredits.nextCreditId;
    assert.equal(credit, "verify-credit");
    const consumed = yield* api.consume(hub, codex.id, credit);
    assert.deepEqual(consumed, { outcome: "reset" });
    assert.deepEqual(mutations, ["consume", "reset-quota"]);
  }).pipe(Effect.provideService(HttpClient.HttpClient, http), Effect.provide(NodeCrypto.layer)),
);
