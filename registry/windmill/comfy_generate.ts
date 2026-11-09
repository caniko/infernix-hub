// infernix-hub generation procedure (version 1).
//
// Input is the hub's `GenerationJob` (crates/domain/src/ports.rs). For each
// variant it submits the bound ComfyUI API graph with a hub-chosen `prompt_id`
// and immediately posts a `comfy_prompt` receipt to the hub.
//
// Ambiguity rule: a `/prompt` request that fails after the body was sent is
// NEVER resubmitted. The procedure first checks `/history/{prompt_id}` and
// `/queue` for the known id; if the prompt is absent it throws and lets the hub
// reconcile. Receipt POSTs are retried (3 attempts, bounded backoff); prompt
// submissions are not.

import * as wmill from "windmill-client";

interface GenerationJob {
  request_version: number;
  generation_request_id: string;
  windmill_path: string;
  engine_instance_id: string;
  engine_base_url: string;
  prompts: unknown[];
  prompt_ids: string[];
  comfy_client_id: string;
  receipt_url: string;
}

interface Receipt {
  kind: "comfy_prompt";
  external_id: string;
  parent_external_id: string;
  engine_instance_id: string;
  raw: unknown;
}

const RECEIPT_ATTEMPTS = 3;
const RECEIPT_BACKOFF_MS = [250, 1000, 2500];
const REQUEST_TIMEOUT_MS = 30_000;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function hubToken(): Promise<string> {
  return await wmill.getVariable("f/infernix_hub/hub_token");
}

async function promptState(baseUrl: string, promptId: string): Promise<"history" | "queue" | null> {
  try {
    const history = await fetch(`${baseUrl}/history/${promptId}`, {
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    if (history.ok) {
      const body = await history.json();
      if (body && typeof body === "object" && promptId in body) return "history";
    }
  } catch {
    // fall through to the queue check
  }
  try {
    const queue = await fetch(`${baseUrl}/queue`, {
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    if (queue.ok) {
      const body = await queue.json();
      const inList = (items: unknown): boolean =>
        Array.isArray(items) && items.some((item) => Array.isArray(item) && item[1] === promptId);
      if (inList(body?.queue_running) || inList(body?.queue_pending)) return "queue";
    }
  } catch {
    // unknown; the caller must fail closed
  }
  return null;
}

async function postReceipt(url: string, token: string, receipt: Receipt): Promise<void> {
  let lastError = "no attempt";
  for (let attempt = 0; attempt < RECEIPT_ATTEMPTS; attempt++) {
    try {
      const response = await fetch(url, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${token}`,
        },
        body: JSON.stringify(receipt),
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      });
      // 409 means the hub already recorded this receipt (idempotent replay).
      if (response.ok || response.status === 409) return;
      lastError = `HTTP ${response.status}: ${await response.text()}`;
    } catch (error) {
      lastError = String(error);
    }
    if (attempt < RECEIPT_ATTEMPTS - 1) {
      await sleep(RECEIPT_BACKOFF_MS[attempt] ?? 2500);
    }
  }
  throw new Error(`comfy_prompt receipt failed after ${RECEIPT_ATTEMPTS} attempts: ${lastError}`);
}

export async function main(job: GenerationJob): Promise<string[]> {
  if (job.request_version !== 1) {
    throw new Error(`unsupported GenerationJob request_version ${job.request_version}`);
  }
  if (job.prompts.length !== job.prompt_ids.length) {
    throw new Error("prompts and prompt_ids must have the same length");
  }
  const token = await hubToken();
  const engineBaseUrl = job.engine_base_url.replace(/\/+$/, "");
  const parentExternalId = process.env.WM_ROOT_FLOW_JOB_ID ?? process.env.WM_JOB_ID ?? "";
  const acceptedIds: string[] = [];

  for (let index = 0; index < job.prompts.length; index++) {
    const knownId = job.prompt_ids[index];

    // Procedure retry safety: if this prompt id already exists on the engine,
    // do not submit it again. Re-post the receipt so a retry after a lost
    // receipt still records the submission (the hub treats duplicates as a
    // no-op).
    const existing = await promptState(engineBaseUrl, knownId);
    if (existing) {
      acceptedIds.push(knownId);
      await postReceipt(job.receipt_url, token, {
        kind: "comfy_prompt",
        external_id: knownId,
        parent_external_id: parentExternalId,
        engine_instance_id: job.engine_instance_id,
        raw: { reconciled: existing, prompt_id: knownId },
      });
      continue;
    }

    let response: Response;
    try {
      response = await fetch(`${engineBaseUrl}/prompt`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          prompt: job.prompts[index],
          client_id: job.comfy_client_id,
          prompt_id: knownId,
        }),
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      });
    } catch (error) {
      // The body may have reached ComfyUI. Reconcile by id; never resubmit.
      const state = await promptState(engineBaseUrl, knownId);
      if (!state) {
        throw new Error(
          `ambiguous /prompt submission for ${knownId}; refusing to resubmit: ${String(error)}`,
        );
      }
      acceptedIds.push(knownId);
      await postReceipt(job.receipt_url, token, {
        kind: "comfy_prompt",
        external_id: knownId,
        parent_external_id: parentExternalId,
        engine_instance_id: job.engine_instance_id,
        raw: { reconciled: state, prompt_id: knownId },
      });
      continue;
    }

    if (!response.ok) {
      // A definitive rejection: no prompt exists, so no receipt.
      throw new Error(`comfy /prompt HTTP ${response.status}: ${await response.text()}`);
    }

    let raw: unknown;
    try {
      raw = await response.json();
    } catch (error) {
      const state = await promptState(engineBaseUrl, knownId);
      if (!state) {
        throw new Error(
          `ambiguous /prompt response for ${knownId}; refusing to resubmit: ${String(error)}`,
        );
      }
      raw = { reconciled: state, prompt_id: knownId };
    }

    const acceptedId =
      raw && typeof raw === "object" && typeof (raw as { prompt_id?: unknown }).prompt_id === "string"
        ? ((raw as { prompt_id: string }).prompt_id)
        : knownId;
    acceptedIds.push(acceptedId);

    await postReceipt(job.receipt_url, token, {
      kind: "comfy_prompt",
      external_id: acceptedId,
      parent_external_id: parentExternalId,
      engine_instance_id: job.engine_instance_id,
      raw,
    });
  }

  return acceptedIds;
}
