# Windmill procedures

Two Bun/TypeScript procedures for the `studio` workspace (Windmill CE 1.839.0,
`http://127.0.0.1:8040`). The hub runs them asynchronously by registry path;
`path@hash` inputs use `POST /api/w/{w}/jobs/run/h/{hash}` so a redeploy cannot
change the code a queued job executes.

| Path | Input | Purpose |
| --- | --- | --- |
| `f/infernix_hub/comfy_generate` | `GenerationJob` | Submit ComfyUI prompts and post `comfy_prompt` receipts |
| `f/infernix_hub/publication_timer` | `PublicationTimer` | Dispatch an armed publication timer to the hub |

## Required variable

Create one **secret** Windmill variable before the first run:

```
f/infernix_hub/hub_token
```

Both procedures read it with `wmill.getVariable` and send it as
`Authorization: Bearer <token>` to the hub's receipt/dispatch endpoints. Do not
put the token in a script, a schedule argument, or a log line.

## Deploy

From a checkout of this repository with a logged-in `wmill` CLI:

```
wmill sync push
```

**Do not run `wmill sync push` from an agent session.** Deployment changes live
Windmill state; it needs operator authorization. The same applies to creating
or activating schedules: **production schedules must not be activated without
authorization**, and this repository does not contain schedule definitions.

## Procedure version 1 contracts

`comfy_generate`:

* one `POST {engine_base_url}/prompt` per variant with
  `{prompt, client_id, prompt_id}`; the hub-chosen UUID is honoured by
  ComfyUI 0.37.0;
* an immediate `POST receipt_url` with
  `{kind:"comfy_prompt", external_id, parent_external_id, engine_instance_id, raw}`;
* receipt POSTs retry 3 times with bounded backoff (250 ms, 1 s, 2.5 s), and a
  409 is treated as an already-recorded receipt;
* an ambiguous `/prompt` failure is **never** resubmitted. The procedure checks
  `/history/{prompt_id}` and `/queue` for the known id and fails closed if the
  prompt is absent.

`publication_timer`:

* `POST dispatch_url` with `{schedule_revision, trigger_id: WM_JOB_ID}`;
* HTTP 409 `stale_schedule_revision` or `already_claimed` is a successful no-op
  and returns `{status:"noop", http_status:409, code, body}`;
* the hub, not Windmill, decides whether a publication may run.
