// fleetix-hub synchronous humanizer preview (version 1).
// Run on draft text only, before approval. This script does not save or send it.

import * as wmill from "windmill-client";

const REQUEST_TIMEOUT_MS = 180_000;

export async function main(
  text: string,
  locked_facts: string[] = [],
  max_chars?: number,
  best_of_n?: number,
) {
  const original = {
    applied: false,
    source_text: text,
    humanized_text: text,
    judge_verified: false,
    fallback_paragraphs: [] as number[],
    paragraphs: null as unknown[] | null,
  };
  const reject = (reason: string) => ({ ...original, reason });

  if (typeof text !== "string" || !text.trim()) {
    return reject("text must not be empty");
  }
  if (!Array.isArray(locked_facts) || !locked_facts.every((fact) => typeof fact === "string")) {
    return reject("locked_facts must be an array of strings");
  }
  if (max_chars != null && (!Number.isSafeInteger(max_chars) || max_chars < 1)) {
    return reject("max_chars must be a positive integer");
  }
  if (best_of_n != null && (!Number.isInteger(best_of_n) || best_of_n < 1 || best_of_n > 64)) {
    return reject("best_of_n must be between 1 and 64");
  }

  try {
    const token = (await wmill.getVariable("f/fleetix_hub/humanizer_token").catch(() => undefined))?.trim();
    const base = await wmill.getVariable("f/fleetix_hub/humanizer_url").catch(() => undefined);
    const url = new URL(base?.trim() || "http://127.0.0.1:3000");
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
      return reject("invalid humanizer base URL");
    }
    if (!token && !["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)) {
      return reject("humanizer token is required for non-loopback URLs");
    }
    url.pathname = `${url.pathname.replace(/\/+$/, "")}/api/v1/humanize`;
    const request: RequestInit = {
      method: "POST",
      headers: {
        "content-type": "application/json",
        ...(token ? { authorization: `Bearer ${token}` } : {}),
      },
      body: JSON.stringify({ text, locked_facts, max_chars, best_of_n }),
      redirect: "error",
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    };
    let response = await fetch(url, request);
    if (response.status === 503) {
      await response.body?.cancel();
      await new Promise<void>((resolve) => setTimeout(resolve, 500));
      response = await fetch(url, request);
    }
    if (response.status !== 200) {
      await response.body?.cancel();
      return reject(response.status === 503
        ? "humanizer is busy or temporarily unavailable (HTTP 503)"
        : `humanizer request failed (HTTP ${response.status})`);
    }
    const output = await response.json().catch(() => null);
    if (!output || typeof output.humanized_text !== "string"
        || typeof output.judge_verified !== "boolean"
        || !Array.isArray(output.fallback_paragraphs)
        || !output.fallback_paragraphs.every((index: unknown) =>
          typeof index === "number" && Number.isSafeInteger(index) && index >= 0)) {
      return reject("humanizer returned an invalid response");
    }

    const rewrite: string = output.humanized_text;
    original.judge_verified = output.judge_verified;
    original.fallback_paragraphs = output.fallback_paragraphs;
    original.paragraphs = Array.isArray(output.paragraphs) ? output.paragraphs : null;
    if (!original.judge_verified) return reject("rewrite was not verified by the judge");
    if (original.fallback_paragraphs.length) return reject("rewrite used fallback paragraphs");
    if (!rewrite.trim()) return reject("rewrite was empty");
    if (max_chars != null && [...rewrite].length > max_chars) return reject("rewrite exceeded max_chars");
    if (locked_facts.some((fact) => fact.trim() && text.includes(fact) && !rewrite.includes(fact))) {
      return reject("rewrite dropped a locked fact");
    }
    return { ...original, applied: true, humanized_text: rewrite, reason: null };
  } catch {
    // Do not echo draft content, response bodies, URLs or credentials into logs.
    return reject("humanizer backend unavailable");
  }
}
