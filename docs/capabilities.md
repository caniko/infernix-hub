# Integration capabilities

Status terms:

- **built** — adapter code exists in `crates/integrations/src/live/`.
- **fixture-tested** — the matching `Fixture*` adapter exists in `fixture.rs`;
  those adapters are lead-owned and are not evidence for the live adapter.
- **contract-verified** — request/response shapes checked against a downloaded
  OpenAPI document or the deployed server source.
- **live-verified** — a read-only request was actually made to the deployed
  service. No mutating request was made anywhere.

No live adapter method below is marked fixture-tested: the fixture adapters in
`fixture.rs` are lead-owned and were not run as evidence for this track.

All observations are from `atlas`, 2026-11-09.

## Paperclip

| | |
| --- | --- |
| Version | `0.3.1-unstable-2026-10-23`, commit `6f1bf1754c4862cbae59a58306652ded8a44cd17` |
| URL | `http://127.0.0.1:3117` |
| Auth | Board API key, agenix `paperclip-operations-operator-key` |

Adapter: `live/paperclip.rs` (`PaperclipClient`). Fixture counterpart:
`FixturePaperclip` in `fixture.rs` (lead-owned; not exercised by this track).

| Method | State |
| --- | --- |
| `create_issue` | built, contract-verified |
| `create_approval` | built, contract-verified |
| `get_approval` | built, contract-verified |

Contract evidence: `docs/contracts/paperclip-openapi-subset.json` (extracted
from the live `GET /api/openapi.json`, 823 paths). The issue-create body is
untyped in OpenAPI; fields come from the same commit's bundled
`createIssueSchema`/`createApprovalSchema` in the Paperclip store
(`lib/paperclip/cli/dist/index.js`).

Field mapping:

- `IssueRequest.company_id` → path `{companyId}`; `title`/`description` →
  same; `origin_key` → `idempotencyKey` (server replays the original issue for
  a repeated key; trimmed, 1–255 chars). `status` is omitted so the server
  default applies.
- `ApprovalRequest` → `POST /api/companies/{companyId}/approvals` with
  `type: "request_board_approval"` (the general board approval type) and
  `payload: {title, summary, release_revision_id, payload_hash, actions,
  requested_by}`; `issue_id` → `issueIds: [id]`.
- `get_approval` maps `status`: `pending`→Pending, `approved`→Approved,
  `rejected`→Rejected, `revision_requested`→RevisionRequested,
  `cancelled`→Cancelled; unknown values fail closed as `Unsupported`.
  `decidedByUserId` is the human board user Paperclip records, never the API
  key identity. `decidedAt` is parsed as RFC3339. `payload_hash` and
  `release_revision_id` are read from `payload`.

Live-verified: `GET /api/health` and `GET /api/openapi.json` (read-only).
Blocker: the board key is an agenix secret and was not read, so no authenticated
call was possible. Approval create has no idempotency field in 0.3.1; a retried
create must be reconciled by reading the approval the hub recorded.

## Windmill

| | |
| --- | --- |
| Version | `CE v1.839.0` |
| URL | `http://127.0.0.1:8040`, workspace `studio` |
| Auth | Workspace bearer token (`paperclip-studio`) |

Adapter: `live/windmill.rs` (`WindmillClient`). Fixture counterpart:
`FixtureWindmill` in `fixture.rs` (lead-owned; not exercised by this track).

| Method | State |
| --- | --- |
| `run_generation` | built, contract-verified |
| `schedule_publication` | built, contract-verified |
| `job` | built, contract-verified |
| `cancel_job` | built, contract-verified |

Contract evidence: live `GET /api/openapi.yaml` (servers `/api`), downloaded to
the evidence directory. Verified: `POST /w/{workspace}/jobs/run/p/{path}` →
`201 text/plain` job uuid with optional `scheduled_for` (RFC3339);
`POST /w/{workspace}/jobs/run/h/{hash}` for `path@hash`;
`GET /w/{workspace}/jobs_u/get/{id}` → `CompletedJob | QueuedJob`;
`POST /w/{workspace}/jobs_u/queue/cancel/{id}` → `200 text/plain`.

Version pinning: a `windmill_path` containing `@` is split on the last `@` and
run through `run/h/{hash}`; otherwise `run/p/{path}`. This keeps a queued job
on the exact script version the hub chose.

Live-verified: `GET /api/version`. No job was started or cancelled. Blocker:
the workspace token was not read, so run/status/cancel were not exercised end
to end.

## ComfyUI

| | |
| --- | --- |
| Version | `0.37.0` (frontend `1.52.7`) |
| URL | `http://127.0.0.1:8188` |
| Auth | none (loopback / wg-home only) |

Adapter: `live/comfy.rs` (`ComfyClient`). Fixture counterpart: `FixtureComfy`
in `fixture.rs` (lead-owned; not exercised by this track).

| Method | State |
| --- | --- |
| `outcome` | built, contract-verified |
| `fetch` | built, contract-verified |
| `cancel` | built, contract-verified |
| `subscribe` | built, contract-verified |

Contract evidence: deployed source
`/nix/store/88s35yl2nv1rkb8cqymmkgmrki7qhkv3-comfyui-0.37.0/share/comfyui/`
(`server.py`, `execution.py`, `comfy_execution/progress.py`). Verified:
`POST /prompt` honours a client-supplied UUID `prompt_id` and `client_id`;
`GET /history/{id}` is `{}` when unknown, else `{prompt, outputs, status:
{status_str, completed, messages}}`; `GET /queue` returns
`queue_running`/`queue_pending` tuples with the prompt id at index 1;
`POST /queue {"delete":[ids]}`; `POST /interrupt {"prompt_id": id}` is
targeted (never sent without `prompt_id`); `/ws?clientId=` emits
`{"type","data"}` JSON plus binary preview frames.

Live-verified: the ignored test
`live::comfy::tests::live_probe_system_stats_and_queue` passed against
`http://127.0.0.1:8188` (`GET /system_stats`, `GET /queue`); the same two GETs
were repeated with curl. No prompt was submitted. Blocker: none for reads;
mutating calls were forbidden.

`OutputFile.kind` is the output category (`images`, `gifs`, `videos`, …).
`fetch` requests `/view` with `type=output` because the port type does not keep
the history entry's per-file `type`; queue positions are 0-based indices in
`queue_pending`.

## n8n

| | |
| --- | --- |
| Version | `2.39.6` |
| URL | `http://127.0.0.1:5680` |
| Auth | webhook header `X-Studio-Token`; REST needs `X-N8N-API-KEY` (not provisioned) |

Adapter: `live/n8n.rs` (`N8nClient`), adapted from the MIT donor (see
`docs/provenance-integrations.md`). n8n has no port trait, so there is no
fixture counterpart.

| Method | State |
| --- | --- |
| `trigger_webhook` | built, not contract-verified |
| `get_json`/`post_json`/`patch_json`/`put_json`/`delete` | built, **unverified** |

Live-verified: `GET /healthz` → `{"status":"ok"}`. No webhook was triggered and
no REST call was made. Blockers: no `X-N8N-API-KEY` exists on this deployment;
the `X-Studio-Token` value is an agenix secret and was not read. REST methods
return `InvalidInput` when no API key is configured instead of sending an
unauthenticated request.

## Postiz

**Not deployed** — no module, package, secret, endpoint or DNS record.

Adapter: `live/postiz.rs` (`PostizClient`). Fixture counterpart:
`FixturePublisher` in `fixture.rs` (lead-owned; not exercised by this track).

| Method | State |
| --- | --- |
| `submit` | built, not contract-verified |
| `lookup` | returns `Unsupported` |
| `reply` / `lookup_reply` | return `Unsupported` |

Built from the public API docs (fetched 2026-11-09): `POST /posts` with
`type: "now"` (the hub never schedules inside Postiz); media via
`POST /upload` multipart and `posts[].value[].image[]`. The docs show no
idempotency key or custom-field filter on `GET /posts`, so an ambiguous submit
requires manual reconciliation; there is no reply endpoint. Accepted and
ambiguous posts are resolved by an operator (`POST /v1/intents/{id}/resolve`).
The documented upload/post payloads carry no media alt text, so `submit`
refuses media with alt text (`Unsupported`) rather than publishing the
approved revision without it. Blocker: Postiz does not exist in the fleet, so
nothing can be contract- or live-verified.

## Windmill procedures

`registry/windmill/comfy_generate.ts` and `publication_timer.ts` are written
with version-1 metadata. **Not deployed**: `wmill sync push` was not run and no
schedule was created. Required secret variable: `f/fleetix_hub/hub_token`. See
`registry/windmill/README.md`.
