# Handoff

Atlas runs `canix-hub` as `fleetix-hub.service`, with most integrations in
fixture mode. See `VISION.md` for the product, `docs/capabilities.md` for
integration evidence, and `docs/nix.md` for the flake and NixOS module.

## What the product is today

- **Hub.** One Axum API (`/v1`), one domain command layer, an outbox worker and
  an audit log over PostgreSQL. Content flows brief → generation → assets →
  release → approval → publication intent → channel.
- **Generation backends.** A Windmill procedure drives ComfyUI;
  `workflows.pipeline_id` selects backends through the `Pipeline` port
  (submit/status/cancel/find). A committed claim makes submission at-most-once.
  If the handle was lost, the hub uses `Pipeline::find` to adopt the run.
- **Packs.** Compile-time extensions add channels, pipelines, outbox topics,
  `/v1/p/<id>` routes, status rows, workflow versions and MCP tools. Upstream
  installs none; product binaries like `canix-hub` install their own, using
  `packs.json` and systemd credentials.
- **Channels.** Dispatch uses only the account transport frozen in the
  release: `fixture`, `postiz`, `disabled`, `n8n:<platform>`, or a pack channel
  such as `plinth`. An unknown transport stops the intent for an operator;
  there is no fallback. Real channels require live Paperclip approvals.
- **Notifications.** A PostgreSQL table stores deduplicated conditions and their
  acknowledgement, resolution and snooze state. `GET /v1/notifications` returns
  a snapshot without `after`, or a cursor feed with `after` and optional `wait`.
  Producers sync Today and fleet conditions and summary staleness. MCP
  `hub_post_summary` writes a daily notification keyed `summary:<YYYY-MM-DD>`.
- **Status feeds.** The hub polls project JSON status documents (S3, SigV4).
  Each failing check raises `monitor:<feed>:<check>`, which resolves on
  recovery. A stale or unreadable feed raises
  `monitor:<feed>:feed`. Feed states are in `/v1/status`.
- **Renewals.** Seeds include procedures and HTTPS provider URLs. Operators
  mark renewals complete and set the next due date.
- **Clients.** The Dioxus console has a Notifications workspace. `hubctl` and
  MCP expose notification operations. The `fleetix-hub-tray` desktop daemon
  provides a tray icon, long-poll feed, and desktop notifications.

## Run it

These commands use fixtures and call no external services.

```sh
nix develop                                    # harbor-rs devshell (treefmt, sqlx, psql)
HUB_PG_DIR=/tmp/hubpg HUB_PG_PORT=54391 scripts/dev-db.sh   # throwaway PostgreSQL under /tmp
eval "$(HUB_PG_DIR=/tmp/hubpg HUB_PG_PORT=54391 scripts/dev-db.sh)"   # exports DATABASE_URL
cargo run -p fleetix-hubd -- dev --database-url "$DATABASE_URL"   # API + worker, fixtures, demo data
HUB_URL=http://127.0.0.1:8470 HUB_TOKEN=dev-operator-token cargo run -p fleetix-hub-desktop
HUB_URL=http://127.0.0.1:8470 HUB_TOKEN=dev-operator-token cargo run -p hubctl -- today
```

Do not point `dev-db.sh` at `var/pg` from this dev shell. Its PostgreSQL 16
cannot start the data directory initialized by PostgreSQL 18.

`dev` prints `dev-operator-token`, `dev-agent-token`, `dev-procedure-token`,
and `dev-ingest-token`. Decide fixture approvals with
`POST /v1/fixtures/paperclip/approvals/{id}/decide` (fixture mode only).

Production-style commands: `serve [--no-worker] [--packs-config FILE]`,
`worker`, `migrate [--check]`, `token --name N --role R [--token-file F]`.
Configuration is environment only (`crates/api/src/bootstrap.rs`); secrets come
from `*_TOKEN_FILE`. Modes: `HUB_PAPERCLIP|HUB_WINDMILL|HUB_COMFY|HUB_ASSISTANT`
= `fixture|live`, `HUB_PUBLISHER` = `fixture|live|disabled`. `N8N_PUBLISH_URL` +
`N8N_PUBLISH_PLATFORMS` + `N8N_WEBHOOK_TOKEN_FILE` add one `n8n:<platform>`
channel each.

`POST /v1/humanize` lets operator and agent tokens preview rewrites without
saving the draft. `HUB_HUMANIZER` accepts `disabled` (default), `fixture`, or
`live`. The live adapter uses `HUMANIZER_URL` (default
`http://127.0.0.1:3000`), optional `HUMANIZER_TOKEN_FILE` (preferred) or
`HUMANIZER_TOKEN`, and `HUMANIZER_TIMEOUT_SECS` (default 180).
`HUB_HUMANIZER_AUTO_APPLY=true` enables verified rewriting of assistant
replies before review; previews don't require it. Failures keep the original text.

The `/v1/personas/{id}` Soul endpoints manage memories, written/spoken voice
exemplars, and daily persona state. Thread drafts use persona-wide and
exact-thread memories, never sandbox entries. Recall and listing can opt into
sandbox; novelty checks use only aired memories.

## Checks

```sh
HUB_TEST_DATABASE_URL="$DATABASE_URL" cargo check --workspace --all-targets
HUB_TEST_DATABASE_URL="$DATABASE_URL" cargo test --workspace   # DB tests skip without the URL
treefmt <files>
```

Journey tests cover:

- Approval invalidation on content changes; stale and duplicate timers;
  fail-closed early timers, pauses, and unverifiable approvals.
- Partial multi-account failure, lost publish response reconciliation without
  resubmission, tracking accepted posts to published, and unreachable publisher retry.
- Finding lost generation receipts by hub-chosen prompt ID, targeted cancel,
  GPU admission and cancel while waiting, and asset retention after engine
  history cleanup.
- Human takeover ahead of a queued agent reply, and chat suggestions staying proposals.
- Pipeline at-most-once submission, crash-window adoption and lookup outage,
  manifest/upload agreement and deadline, pack routes, pack workflows and
  `generate`, and a procedure downloading asset bytes.

This pass did not run clippy, the flake's Nix checks (module eval, packaging),
live integrations, or the CI workflow another stream added under `.github/workflows`.

## Capability levels

| Area | Built | Fixture-tested | Contract-verified | Live-verified |
| --- | --- | --- | --- | --- |
| Hub API, domain, worker, outbox, audit | yes | yes (journey) | — | — |
| Packs (channels, pipelines, topics, routes, workflows, MCP) | yes | yes | — | see deployment configuration |
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
| NixOS module + pack options | yes | eval checks | — | Atlas service activated |

## Live integration blockers observed in the 2026-11-13 assessment

1. **ComfyUI has no checkpoints.** The atlas instance is the TRELLIS workbench;
   `poster-txt2img` needs a checkpoint. `registry/comfyui/swatch.v1.json` is
   model-free and is the only workflow that could run today.
2. **Paperclip:** No authenticated calls or reads of the board key
   (`paperclip-operations-operator-key`). Set `HUB_COMPANY_ID`; the company ID
   was unknown. Approval create has no idempotency key in 0.3.1: ambiguous
   creates go to the dead-letter queue; operators use hash-checked `link-approval`.
3. **Windmill:** procedures not deployed (`wmill sync push` not run); secret
   variable `f/fleetix_hub/hub_token` not created; workspace token not read.
4. **n8n:** no `X-N8N-API-KEY`; the `X-Studio-Token` webhook secret was not read;
   no workflow is imported and none points at `/v1/ingest/news` or the publish
   contract.
5. **Postiz:** Not deployed. The adapter refuses media with alt text (no
   documented alt field); confirm one before enabling image posts. Lookup is
   unsupported; operators resolve accepted/ambiguous posts
   (`POST /v1/intents/{id}/resolve`).

## Deployment qualification

The Atlas deployment does not qualify external integrations. The findings
above come from the earlier review.

## Review

Sol (`openai/gpt-6.1-sol`, xhigh) approved the earlier tree in round 4. Round 1
found 14 issues (3 blockers); `1ce7dcc`, `3b78006` and `5d6b3f1` fixed or
narrowed all of them. The later changes (license, approved state, procedure
status, polish, harbor bump) also passed in round 4 after lock-order fixes in
`e418f03`, `f51a350` and `a527cc6`.

The worker holds the outbox row lock across remote calls. A crash after a
non-idempotent call but before recording its effect can replay it. Approvals and
generations check their recorded effect first; the worker adopts pipeline claims
instead of resubmitting. A lease column is the upgrade path.

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

`design/fleetix-hub.fig` (OpenPencil) contains D01 Studio, D02 Today, and M01
narrow (390 px). Layout and tokens match screenshots of the running app at
1440 and 760 px. D01 reflects P1–P3 (status bar wording, "Approved" chip,
persona name); D02 already had readable labels. M01 shows the unchanged
selecting stage. The practical desktop window minimum is wider than 390 px,
so there is no app screenshot at that width.

## Open decisions for the owner

- Whether to publish through Postiz or another transport, and whether to import
  the n8n free-platform set.
- Which ComfyUI checkpoint/workflow to register first.
