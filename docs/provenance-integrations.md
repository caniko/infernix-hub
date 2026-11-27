# Integration provenance

All sources below were retrieved on 2026-11-09 from `atlas`.

## Paperclip OpenAPI

| Item | Value |
| --- | --- |
| Source URL | `http://127.0.0.1:3117/api/openapi.json` (unauthenticated) |
| Retrieved | 2026-11-09 |
| Paperclip package | `0.3.1-unstable-2026-10-23` |
| Source commit | `6f1bf1754c4862cbae59a58306652ded8a44cd17` |
| Schema title/version | `Paperclip API` / `1.0.0` |
| Full document | `/data/nvme0/can/ProjectState/evidence/fleetix-hub/paperclip-openapi.json` (823 paths) |
| Subset in repo | `docs/contracts/paperclip-openapi-subset.json` |

The subset keeps only the adapter's paths, the `Error` schema, and both board
security schemes. OpenAPI leaves the issue-create body untyped (`schema: {}`).
The subset's `hub_notes` record the adapter's issue-create field names checked
against `createIssueSchema` from the same commit
(`lib/paperclip/cli/dist/index.js` in the Nix store).
Approval statuses were checked against the bundled `approvals` table and
the commit's `docs/api/approvals.md`.

## Windmill OpenAPI

| Item | Value |
| --- | --- |
| Source URL | `http://127.0.0.1:8040/api/openapi.yaml` (unauthenticated) |
| Retrieved | 2026-11-09 |
| Server version | `CE v1.839.0` (`GET /api/version`); spec front matter lags at `1.740.0` |
| Document | `/data/nvme0/can/ProjectState/evidence/fleetix-hub/windmill-openapi.yaml` (1,278,691 B, servers `/api`) |

Verified against the document: `runScriptByPath` (`201 text/plain`
uuid, `scheduled_for` query), `runScriptByHash`
(`/jobs/run/h/{hash}`), `getJob` (`CompletedJob | QueuedJob`), and
`cancelQueuedJob` (`200 text/plain`). No authenticated Windmill call was made.

## ComfyUI source

| Item | Value |
| --- | --- |
| Version | `0.37.0` (`GET /system_stats`) |
| Source root | `/nix/store/88s35yl2nv1rkb8cqymmkgmrki7qhkv3-comfyui-0.37.0/share/comfyui/` |
| Files read | `server.py`, `execution.py`, `comfy_execution/progress.py` |

`/prompt` accepts a validated client `prompt_id` and
`client_id`; `/history/{id}` returns `{}` when unknown; `/queue` items are
tuples with the prompt id at index 1; `/interrupt {prompt_id}` is targeted;
`/ws` frames are `{"type","data"}` JSON with binary preview frames alongside.
The adapter's recorded samples in `crates/integrations/src/live/testdata/` are
derived from these shapes.

## n8n donor

| Item | Value |
| --- | --- |
| Donor path | `/data/nvme0/can/ProjectState/donors/aivyx-pa/src/aivyx-pa-8097870bdad7c1a726448eeeeb82331027f1e6cf/crates/aivyx-n8n/src/n8n_client.rs` |
| License | MIT |
| sha256 | `2f012e594ff0cc76639d9e33087742cf5ab7533540c61435f44bae3a08858f97` |
| Destination | `crates/integrations/src/live/n8n.rs` |

* Replaced `N8nConfig`'s derived `Debug` with a manual one that prints
  `<redacted>` for both credentials. `SecretString` wraps secrets and has no
  `Display` and a redacting `Debug`.
* Made `n8n_api_key` optional for the deployed webhook-only configuration;
  REST methods return `InvalidInput` without it.
* Added `webhook_token`, `webhook_url`, `trigger_webhook` (`POST
  /webhook/<path>` with `X-Studio-Token`).
* Added `N8nClientError::Ambiguous` for failures after a webhook body was sent;
  a trigger is never blindly retried.
* Kept the donor's base-URL trimming, `url_for`, `decode_response`, REST
  methods, `delete` semantics, and URL/error tests.
* Dropped the donor's TOML config test (this crate has no `toml` dependency).

The REST half remains unverified: the deployment has no `X-N8N-API-KEY`.

## Postiz docs

* <https://docs.postiz.com/public-api/introduction> (fetched 2026-11-09)
* <https://docs.postiz.com/public-api/posts/create> (`type: now`, request and
  response shapes)
* <https://docs.postiz.com/public-api/posts> (`GET /posts` has no idempotency
  or custom-field filter)
* <https://docs.postiz.com/public-api/uploads/upload-file> (`POST /upload`
  multipart, `{id, path}` response)

Postiz is not deployed, so none of this is contract-verified against a server.

## Requests to the lead

* No port-trait changes are needed.
* `crates/integrations/Cargo.toml` gained `tokio-tungstenite` `0.29`
  (`default-features = false`, features `connect`, `handshake`) and
  `futures-util` (workspace) for the ComfyUI `/ws` subscriber. No other
  dependency changes were made.
* Postiz uploads build their own `multipart/form-data` body because workspace
  `reqwest` lacks the `multipart` feature. Enabling it would allow
  `reqwest::multipart` instead.
