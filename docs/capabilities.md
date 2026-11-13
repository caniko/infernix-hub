# Integration and platform capabilities

What the hub can do, what has actually been proven, and what does not exist.
Nothing in this file is deployed: see [Not deployed](#not-deployed).

## Status terms

- **built** — the code exists and compiles.
- **fixture-tested** — a `Fixture*` adapter in `crates/integrations/src/fixture/`
  or the in-process stand-in was exercised end to end. It is evidence for the
  hub's own machinery, never for the live adapter.
- **contract-verified** — request/response shapes were checked against a
  downloaded OpenAPI document or the deployed server source.
- **live-verified** — a request was actually made to the deployed service. No
  mutating request was made anywhere.

A live adapter is never marked fixture-tested by association: the fixture
adapters are separate code.

## Packs

A pack is a compile-time extension. The trait is `Pack` in
`crates/api/src/plugin.rs`; `fleetix_hubd::run(settings, packs)` installs the
packs it is given once the `Hub` exists. The upstream `fleetix-hubd` binary
passes an empty list, so everything below is inert unless a product binary
(such as `canix-hub`) ships a pack.

`Pack::install` gets a `ServerHost`, which registers:

| Registration | Rule |
| --- | --- |
| `channel(transport, publisher)` | `accounts.transport` name; unique within the pack |
| `pipeline(id, pipeline)` | id must be `<pack id>.…`; unique |
| `topic(id, handler)` | `<pack id>.…`; core topics can never be claimed |
| `routes(router)` | mounted at `/v1/p/<pack id>`, using the core `Caller` extractor |
| `integration(status)` | one status row for `GET /v1/status` |
| `workflow(spec)` | name `<pack id>.…`; one definition per name+version; a `pipeline_id` must name a pipeline this same pack registered |

`install_mcp` gets an `McpHost` and registers tools with an effect. The MCP
server's authorizer allows `Read` and `Write` and denies `Destructive`; the
token's role still decides what the API accepts.

Startup fails, rather than resolving a conflict by order, on: a duplicate pack
id, an id outside `[a-z0-9-]+`, a pack id that collides with a core topic
namespace, a duplicate or unprefixed pipeline/topic/workflow, a workflow naming
an unregistered pipeline, a pack channel colliding with a live built-in or an
earlier pack, and a pipeline or topic registered twice.

Pack workflows are written after the `Hub` exists (that is why they are
declared through `ServerHost` and not through the registry directory). A
workflow version is immutable: re-registering the same name and version with a
different graph or pipeline is refused.

`packs.json` (via `serve --packs-config FILE` or `HUB_PACKS_CONFIG`) is an
object mapping pack id to that pack's config object. Startup fails when the file
enables an id the binary does not register. The pack sees its own config as
`ServerContext.config` (`{}` when absent) and its credential files in
`$CREDENTIALS_DIRECTORY`.

A pack channel posts for real, so it is subject to the live-approval guard in
[Channels](#channels-and-publication-transports) even when the built-in
publisher is a fixture.

## Pipelines

A pipeline is a generation backend behind the `Pipeline` port
(`crates/domain/src/ports.rs`):

| Method | Meaning |
| --- | --- |
| `submit` | start one run; returns an opaque handle |
| `status` | `Pending`, `Running{progress}`, `Deferred{reason}`, `Succeeded{artifacts}`, `Failed{reason}`, `Cancelled` |
| `cancel` | stop the run; `Dequeued`, `Interrupted`, `AlreadyFinished`, `Unsupported` |
| `find` | the run an earlier submission started, if the backend still knows it; default `Ok(None)` |

A workflow row selects it: `workflows.pipeline_id` is set instead of an
`api_graph`, and `engine_instance_id` is only the admission target (its base
URL is never polled as a ComfyUI queue).

Submission is at-most-once and crash-safe:

1. `generate` records the generation, a `pipeline_run` attempt and one outbox
   `pipeline_run` row.
2. `deliver_pipeline` commits a submission claim (`submit_started_at`) in its
   own transaction **before** calling `submit`.
3. A redelivery that sees a recorded run does nothing.
4. A redelivery that sees a claim without a run calls `Pipeline::find`: a run
   comes back and is recorded through the same fenced path a fresh submission
   uses (so a cancel that won the race stops the run instead of adopting it); no
   run fails the attempt with `submission outcome unknown`; a lookup error
   keeps the claim and retries. The work is never submitted twice.
5. `Unreachable`, and `5xx`/`429`, release the claim and retry. `Ambiguous` and
   every other answer fail the attempt and leave the claim.

Follow-up, artifacts and completion:

- `reconcile_pipelines` (every worker pass) reads `status` for open runs, in a
  bounded random batch with a per-call timeout.
- `Deferred` fails the attempt: the hub's admission check is the GPU gate, not a
  pipeline-busy signal.
- `Succeeded` needs a non-empty manifest. Each output is uploaded by the
  procedure to `POST /v1/generations/{id}/artifacts` with `x-artifact-source`,
  `content-type` and `x-sha256` (role `procedure`, 256 MiB cap, sha256
  verified). A replay is accepted only when identical.
- The generation completes only when every manifest row has an uploaded, ready
  asset with a matching digest; otherwise the generation fails at the 30 minute
  upload deadline naming the missing sources.
- Cancelling asks the pipeline to stop the run. The outcome decides what
  follows: `Dequeued` and `Interrupted` end in `Cancelled`; `AlreadyFinished`
  with a recorded manifest still awaiting uploads cancels the attempt and
  abandons those uploads (without a manifest the request stays `cancelling`
  until a status read ends it); `Unsupported` or an error leaves it
  `cancelling` until a later status read or the operator ends it.

What ships here: `fixture.svg`, the in-process fixture pipeline behind
`registry/pipeline/fixture-svg.v1.json`, and `WindmillPipeline`, a generic
adapter that runs one Windmill script per `pipeline_id` and reads its terminal
manifest from the job result. Packs add their own, e.g. the `animation` pack's
four ledger-backed stage pipelines.

## Channels and publication transports

A publishing channel is keyed by `accounts.transport` and implements
`Publisher` (`submit`, `lookup`, `reply`, `lookup_reply`). A release action
freezes the account's transport when the release is frozen; dispatch uses only
that channel, and a transport with no channel fails the intent with
`transport_unavailable` for an operator. There is no fallback to another
provider, ever.

Registered today:

| Name | Adapter | Notes |
| --- | --- | --- |
| `fixture` | in-process fixture publisher | simulates a platform; also aliases seeded account transports in fixture mode |
| `postiz` | `live/postiz.rs` | built from public docs; Postiz is not deployed |
| `disabled` | no-op | `HUB_PUBLISHER=disabled` |
| `n8n:<platform>` | `live/n8n.rs` | one channel per platform workflow, when `N8N_PUBLISH_PLATFORMS` is set |
| pack channels | pack-provided `Publisher` | e.g. `plinth` from the `canix-hub` pack |

Only `HUB_PUBLISHER=fixture` aliases the seeded transports to the in-process
adapter. A live publisher, every `n8n:<platform>` channel and every pack channel
posts for real, so each requires `HUB_PAPERCLIP=live`: fixture approvals never
authorize a real post, and startup fails naming the channel otherwise.

## The n8n contract (v1)

One versioned webhook serves every platform. Contract source of truth is
`studio-pipeline/n8n/README.md`; the only caller is `N8nPublisher`.

- `POST {base}/webhook/fleetix-hub/v1/<platform>` as `multipart/form-data`:
  a JSON `request` part (idempotency key, frozen text and `platform_settings`,
  and per media `sha256`/`mime`/`alt_text`) plus one binary part per media item,
  which the workflow verifies against its `sha256` before posting.
- `Authorization: Bearer <N8N_WEBHOOK_TOKEN>`; no hub asset URL or extra token
  travels to n8n.
- Idempotency is claim-first in n8n's Postgres: insert
  `(platform, key) → 'pending'` on conflict do nothing before any platform call.
  A won claim posts and records `published` (provider id/url) or `failed`; a lost
  claim returns the existing row.
- `GET {base}/webhook/fleetix-hub/v1/<platform>/lookup?key=` answers `absent`,
  `pending`, `published` or `failed`. `pending` is **uncertain** (crash between
  claim and result) and stops for the operator; only `absent` means the provider
  has no record. A workflow without lookup makes the channel answer
  `Unsupported`.
- `reply_to` is honoured only by the `mastodon` channel (as
  `in_reply_to_id`); other platforms answer `Unsupported` rather than posting a
  new standalone status.
- No per-post metrics in v1.

State: built; the publisher's request shapes and claim mapping are unit-tested
against a recording server. **Not verified against a live n8n**: the workflow
exports live in `studio-pipeline/n8n/` marked unverified, no `X-N8N-API-KEY`
exists on this deployment, and no workflow points at the hub yet.

## Plinth through a pack

The hub ships no Plinth code. Plinth is reached as a pack channel: a downstream
binary (`canix-hub`) registers a `plinth` `Publisher` that creates the article
through Plinth's admin API, and the NixOS module wires its configuration and
credential:

```nix
services.fleetix-hub.packs.plinth = {
  enable = true;
  config = {baseUrl = "https://example.com";};
  credentials.token = config.age.secrets.plinth-token.path;
};
```

That renders `packs.json` and a `<id>-<name>` systemd credential; the pack reads
the token from `$CREDENTIALS_DIRECTORY`. Because it is a pack channel, it posts
for real and therefore requires live Paperclip approvals. State: the pack
contract and the module options are tested; the Plinth channel itself lives in
the `canix-hub` repository and is not deployed.

## Live adapters: evidence

### Paperclip

Version `0.3.1-unstable-2026-10-23`, commit `6f1bf1754c4862cbae59a58306652ded8a44cd17`.
Adapter `live/paperclip.rs`. `create_issue`, `create_approval` and
`get_approval` are contract-verified against `docs/contracts/paperclip-openapi-subset.json`
(extracted from the live `GET /api/openapi.json`) plus the `createIssueSchema`/
`createApprovalSchema` in the same commit's store. `origin_key` → `idempotencyKey`
on issue create; `get_approval` maps every status and fails closed on unknown
values; `decidedByUserId` is the human board user, never the API key identity.
Live-verified: `/api/health` and `/api/openapi.json` reads only. The board key
is an agenix secret and was not read, so no authenticated call was made.
Approval create has no idempotency field in 0.3.1: an ambiguous create is
dead-lettered and the operator links the existing approval (hash-checked).

### Windmill

`CE v1.839.0`, workspace `studio`. Adapter `live/windmill.rs`: `run_generation`,
`schedule_publication`, `job`, `cancel_job`, contract-verified against the live
OpenAPI. A `windmill_path` containing `@` is split on the last `@` and run
through `/run/h/{hash}`, so a pinned version is honoured. `WindmillPipeline`
reuses the same client for pipeline workflows. Live-verified: `GET /api/version`
only; the workspace token was not read and no job was started or cancelled.

### ComfyUI

`0.37.0`. Adapter `live/comfy.rs`: `outcome`, `fetch`, `cancel`, `subscribe`,
`load`, contract-verified against the deployed server source. `POST /prompt`
honours a hub-chosen `prompt_id` and `client_id`; `GET /history/{id}` is `{}`
when unknown; `/ws` events are progress hints only and terminal state comes from
history. Live-verified against `127.0.0.1:8188`: `GET /system_stats` and
`GET /queue`. No prompt was submitted.

### n8n

`2.39.6`. Adapter `live/n8n.rs` (see the contract above). Live-verified:
`GET /healthz` only.

### Postiz

Adapter `live/postiz.rs`, built from public docs: `POST /posts` with
`type: "now"`, media via `POST /upload`. No idempotency key or custom-field
filter is documented, and there is no reply endpoint, so `lookup` returns
`Unsupported` and accepted or ambiguous posts are resolved by an operator
(`POST /v1/intents/{id}/resolve`). The documented upload/post payloads carry no
media alt text, so `submit` refuses media with alt text instead of publishing
without it.

## Fixture coverage

Fixture mode is what `dev` and every test runs. It exercises the hub's own
machinery end to end: briefs, Paperclip issues and approvals, the Windmill
procedure and publication clock, ComfyUI generation and asset import, the
pipeline submission/status/cancel paths including the crash-window adoption,
release freezing, channel dispatch, engagement replies, MCP, the desktop and
pack registration. It says nothing about a live service.

The desktop shows a `FIXTURE MODE` banner whenever any adapter is simulated,
and `GET /v1/status` reports the mode and verification level per integration.

## Not deployed

- `fleetix-hubd` / `canix-hub` services: no host imports the NixOS module. No
  database, token, DNS record or systemd unit exists for the hub.
- Windmill procedures `registry/windmill/*.ts`: `wmill sync push` was not run and
  no schedule was created.
- Packs (including `plinth` and `animation`): compiled in a downstream binary
  that is not deployed; no `packs.json` is installed on any host.
- n8n workflows: not imported, no API key, no webhook pointed at the hub.
- Paperclip approvals: never created against the live board.
- Postiz: no module, package, secret, endpoint or DNS record.

No release was published, no engagement message was sent, and no live mutating
request was made anywhere.
