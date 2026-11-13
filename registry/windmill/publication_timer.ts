// fleetix-hub publication timer procedure (version 1).
//
// Input is the hub's `PublicationTimer` (crates/domain/src/ports.rs). The
// procedure posts the armed revision to the hub's dispatch endpoint. The hub,
// not Windmill, owns all publishing decisions; a stale revision or an already
// claimed dispatch is a successful no-op.

import * as wmill from "windmill-client";

interface PublicationTimer {
  intent_id: string;
  schedule_revision: number;
  due_at: string;
  dispatch_url: string;
}

const REQUEST_TIMEOUT_MS = 30_000;

function noopCode(body: unknown): string {
  if (!body || typeof body !== "object") return "";
  const record = body as Record<string, unknown>;
  for (const key of ["code", "error", "reason"]) {
    const value = record[key];
    if (typeof value === "string") return value;
  }
  return "";
}

export async function main(timer: PublicationTimer): Promise<unknown> {
  const token = await wmill.getVariable("f/fleetix_hub/hub_token");
  const triggerId = process.env.WM_JOB_ID ?? "";

  const response = await fetch(timer.dispatch_url, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${token}`,
    },
    // The hub accepts only these two fields; intent identity travels in the
    // job args and is re-read from its own records.
    body: JSON.stringify({
      schedule_revision: timer.schedule_revision,
      trigger_id: triggerId,
    }),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });

  const text = await response.text();
  let body: unknown = text;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    // keep the raw text for the error/log path
  }

  if (response.status === 409) {
    const code = noopCode(body);
    if (code === "stale_schedule_revision" || code === "already_claimed") {
      return { status: "noop", http_status: 409, code, body };
    }
    throw new Error(`hub rejected dispatch with HTTP 409: ${text}`);
  }
  if (!response.ok) {
    throw new Error(`hub dispatch failed with HTTP ${response.status}: ${text}`);
  }
  return body;
}
