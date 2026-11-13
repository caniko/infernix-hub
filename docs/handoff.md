# Handoff

State on 2026-11-13, branch `feat/fleetix-hub`, open as
[PR #1](https://github.com/caniko/infernix-hub/pull/1) against `trunk`. Nothing
in this tree is deployed. Read `VISION.md` for the product, `docs/capabilities.md`
for per-integration evidence, and `docs/nix.md` for the flake and NixOS module.

## What the product is today

- **Hub.** One Axum API (`/v1`), one domain command layer, an outbox worker and
  an audit log over PostgreSQL. Content flows brief → generation → assets →
  release → approval → publication intent → channel.
- **Generation backends.** ComfyUI through a Windmill procedure, and pipelines
  behind the `Pipeline` port (submit/status/cancel/find) selected by
  `workflows.pipeline_id`. Pipeline submission is at-most-once with a committed
  claim, and a claim whose run handle was lost is adopted through
  `Pipeline::find` instead of resubmitted.
- **Packs.** Compile-time extensions that add channels, pipelines, outbox topics,
  `/v1/p/<id>` routes, status rows, workflow versions and MCP tools. The
  upstream binary installs none; a product binary such as `canix-hub` installs
  its own and is configured by `packs.json` plus systemd credentials.
- **Channels.** A release freezes its account's transport and dispatch uses only
  that channel: `fixture`, `postiz`, `disabled`, `n8n:<platform>`, or a
  pack-provided one such as `plinth`. No fallback; an unknown transport stops
  the intent for an operator. Every real channel requires live Paperclip
  approvals.
- **Clients.** Dioxus desktop (`apps/desktop`), `hubctl`, and an MCP server
  (`fleetix-hub-mcp`, read-only `hub_status`, plus pack Write tools).

## Run it

Everything below runs on fixtures: no external service is called.

```sh
nix develop                                    # harbor-rs devshell (treefmt, sqlx, psql)
HUB_PG_DIR=/tmp/hubpg HUB_PG_PORT=54391 scripts/dev-db.sh   # throwaway PostgreSQL under /tmp
eval "$(HUB_PG_DIR=/tmp/hubpg HUB_PG_PORT=54391 scripts/dev-db.sh)"   # exports DATABASE_URL
cargo run -p fleetix-hubd -- dev --database-url "$DATABASE_URL"   # API + worker, fixtures, demo data
HUB_URL=http://127.0.0.1:8470 HUB_TOKEN=dev-operator-token cargo run -p fleetix-hub-desktop
HUB_URL=http://127.0.0.1:8470 HUB_TOKEN=dev-operator-token cargo run -p hubctl -- today
```

Do not point `dev-db.sh` at `var/pg` from this dev shell: that directory was
initialised by PostgreSQL 18 and the dev shell's PostgreSQL 16 cannot start it.

`dev` prints its tokens (`dev-operator-token`, `dev-agent-token`,
`dev-procedure-token`, `dev-ingest-token`). Fixture approvals are decided with
`POST /v1/fixtures/paperclip/approvals/{id}/decide` (fixture mode only).

Production-style commands: `serve [--no-worker] [--packs-config FILE]`,
`worker`, `migrate [--check]`, `token --name N --role R [--token-file F]`.
Configuration is environment only (`crates/api/src/bootstrap.rs`); secrets come
from `*_TOKEN_FILE`. Modes: `HUB_PAPERCLIP|HUB_WINDMILL|HUB_COMFY|HUB_ASSISTANT`
= `fixture|live`, `HUB_PUBLISHER` = `fixture|live|disabled`. `N8N_PUBLISH_URL` +
`N8N_PUBLISH_PLATFORMS` + `N8N_WEBHOOK_TOKEN_FILE` add one `n8n:<platform>`
channel each.

## Checks

```sh
HUB_TEST_DATABASE_URL="$DATABASE_URL" cargo check --workspace --all-targets
HUB_TEST_DATABASE_URL="$DATABASE_URL" cargo test --workspace   # DB tests skip without the URL
treefmt <files>
```

Last run on this branch (2026-11-13, private PostgreSQL 16 under
`/tmp/hubpg-h1`): `cargo check --workspace --all-targets` clean; `cargo test
--workspace` 151 passed, 1 ignored — desktop 27, `fleetix-hubd` lib 9, journey
45, contracts 3, domain 7, integrations 59 (+1 ignored live ComfyUI probe),
worker 1, MCP/`fleetix-hubd`/`hubctl` binaries 0. No test reported skipping, so
the database-backed tests really ran.

The journey tests cover: changed content cannot reuse an approval, stale and
duplicate timers, early timer / pause / unverifiable approval fail closed,
partial multi-account failure, lost publish response reconciled (never
resubmitted), accepted tracked to published, lost generation receipt found by
the hub-chosen prompt id, targeted cancel, human takeover beats a queued agent
reply, assets survive engine history cleanup, chat suggestions stay proposals,
GPU admission, cancel while awaiting admission, unreachable publisher retry,
pipeline at-most-once submission, the crash-window adoption and its lookup
outage, manifest/upload agreement and its deadline, pack routes, pack workflows
and `generate`, and a procedure downloading asset bytes.

Not run here: clippy, the flake's Nix checks (module eval, packaging), any live
integration, and the CI workflow another stream added under `.github/workflows`.

## Capability levels

| Area | Built | Fixture-tested | Contract-verified | Live-verified |
| --- | --- | --- | --- | --- |
| Hub API, domain, worker, outbox, audit | yes | yes (journey) | — | — |
| Packs (channels, pipelines, topics, routes, workflows, MCP) | yes | yes | — | no pack deployed |
| Pipelines (at-most-once, adoption, artifacts, cancel) | yes | yes | Windmill job API | — |
| Publication channels | yes | yes (fixture publisher) | — | — |
| n8n contract v1 channels | yes | yes (recording server) | no (exports unverified) | no |
| Paperclip issues + native approvals | yes | yes | OpenAPI 0.3.1 | health/OpenAPI reads |
| Windmill procedures + publication clock | yes | yes | OpenAPI CE 1.839.0 | `GET /api/version` |
| ComfyUI submit/history/import/cancel | yes | yes | deployed 0.37.0 source | `/system_stats`, `/queue` |
| GPU admission (llama-swap `/running`) | yes | yes | response shape | — |
| Postiz publisher | yes | yes | no (public docs only) | no (not deployed) |
| n8n news ingest | adapter + `/v1/ingest/news` | ingest route yes | no | `/healthz` |
| Local LLM assistant | yes | fixture assistant | OpenAI-compatible shape | — |
| Desktop (Dioxus 0.8.0-alpha.3) | yes | fixture server, 1440/760 px screenshots | — | — |
| NixOS module + pack options | yes | eval checks | — | not activated |

## Live blockers (exact)

1. **Hub service:** no host imports the module; no database, token, DNS record,
   `packs.json` or systemd unit exists. Activation is an operator step through
   the Canix pin train and rops switch.
2. **Packs:** the `plinth` and `animation` packs live in the `canix-hub` crate in
   the Canix repository. They are not built, installed or enabled anywhere yet.
3. **ComfyUI has no checkpoints.** The atlas instance is the TRELLIS workbench;
   `poster-txt2img` needs a checkpoint. `registry/comfyui/swatch.v1.json` is
   model-free and is the only workflow that could run today.
4. **Paperclip:** no authenticated call made; the board key
   (`paperclip-operations-operator-key`) was not read. Company id unknown — set
   `HUB_COMPANY_ID`. Approval create has no idempotency key in 0.3.1: an
   ambiguous create is dead-lettered and the operator links the approval with
   `link-approval` (hash-checked).
5. **Windmill:** procedures not deployed (`wmill sync push` not run); secret
   variable `f/fleetix_hub/hub_token` not created; workspace token not read.
6. **n8n:** no `X-N8N-API-KEY`; the `X-Studio-Token` webhook secret was not read;
   no workflow is imported and none points at `/v1/ingest/news` or the publish
   contract.
7. **Postiz:** not deployed. When it is, the adapter refuses media that carries
   alt text (the documented API has no alt field) — confirm an alt-text field
   before enabling image posts. Lookup is unsupported, so accepted/ambiguous
   posts are resolved by an operator (`POST /v1/intents/{id}/resolve`).

## Not deployed

Windmill procedures, n8n workflows, packs, the hub service and Postiz. No
schedule was activated, nothing was published, no engagement message was sent,
and no live mutating request was made.

## Review

Four Sol review rounds (`openai/gpt-6.1-sol`, xhigh) on the earlier tree; round 4
approved. Round 1 found 14 issues (3 blockers); all fixed or narrowed in
`1ce7dcc`, `3b78006` and `5d6b3f1`. The later delta (license, approved state,
procedure status, polish, harbor bump) had its own four rounds; round 4 approved
after lock-order fixes in `e418f03`, `f51a350` and `a527cc6`.

Known ceiling: the worker holds the outbox row lock across a remote call; a
crash between a non-idempotent call and its record can still replay it
(approvals and generations check their recorded effect first, and a pipeline
claim is adopted rather than resubmitted). A lease column is the upgrade path.

## Desktop delivery log

| Id | Defect | Status |
| --- | --- | --- |
| D1 | Native `<select>` drawn light with unreadable text (WebKitGTK) | fixed `4033231` |
| D2 | Studio prefilled inputs the workflow does not bind (`unknown_input`) | fixed `4033231` |
| D3 | Window opened at the 800 px default; inspector overlay covered the app | fixed `4033231` |
| D4 | Latest release read from the wrong end of the list | fixed `1ce7dcc` |
| D5 | Canon editor could fill from the previous persona | fixed `1ce7dcc`, `3b78006` |
| P1 | Status bar truncated at 1440 px (repeated "fixture · fixture tested") | fixed: fixture shows the mode, live shows its evidence level |
| P2 | Content list said "In review" after approval | fixed: new `approved` content state; rejection returns to `selecting` |
| P3 | Studio header showed the persona id | fixed: persona name |
| P4 | "3/4 attempts" counted the Windmill root | fixed: counts prompts ("2/2 prompts done") |
| P5 | Windmill root attempt stayed "submitted" after success | fixed: worker follows procedure status (display only) |
| P6 | Today chips showed raw ids (`IN_REVIEW`, `REPLY_REVIEW`) | fixed |

## Designs

`design/fleetix-hub.fig` (OpenPencil): D01 Studio, D02 Today, M01 narrow
(390 px). Checked against screenshots of the running app at 1440 and 760 px;
layout and tokens match. D01 was updated for P1–P3 (status bar wording,
"Approved" chip, persona name); D02 already used readable labels; M01 shows the
selecting stage, which is unchanged. 390 px was not captured from the app
(desktop window minimum in practice is wider).

## Open decisions for the owner

- Postiz vs another transport for publishing; whether the n8n free-platform set
  is worth importing at all.
- Which ComfyUI checkpoint/workflow to register first.
- Which host carries the hub, and whether the first deployment is fixture-mode
  only.
