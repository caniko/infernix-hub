# Handoff

State on 2026-11-10, branch `trunk`, nothing pushed. Read `VISION.md` for the
product, `docs/source-provenance.md` for what was taken from Aivyx PA,
`docs/capabilities.md` for per-integration evidence and `docs/nix.md` for the
flake and NixOS module.

## Run it

Everything below runs on fixtures: no external service is called.

```sh
nix develop                                   # harbor-rs devshell (treefmt, sqlx, psql)
eval "$(scripts/dev-db.sh)"                   # throwaway PostgreSQL 16 under var/pg
cargo run -p infernix-hubd -- dev --database-url "$DATABASE_URL"   # API + worker, fixture adapters, demo data
HUB_URL=http://127.0.0.1:8470 HUB_TOKEN=dev-operator-token cargo run -p infernix-hub-desktop
HUB_URL=http://127.0.0.1:8470 HUB_TOKEN=dev-operator-token cargo run -p hubctl -- today
```

`dev` prints its tokens (`dev-operator-token`, `dev-agent-token`,
`dev-procedure-token`, `dev-ingest-token`). Fixture approvals are decided with
`POST /v1/fixtures/paperclip/approvals/{id}/decide` (fixture mode only). The
desktop shows a "FIXTURE MODE" banner whenever any adapter is simulated.

Production-style commands: `infernix-hubd serve [--no-worker]`, `worker`,
`migrate [--check]`, `token --name N --role R [--token-file F]`. Configuration
is environment only (`crates/api/src/bootstrap.rs`); secrets come from
`*_TOKEN_FILE`. Adapter modes: `HUB_PAPERCLIP|HUB_WINDMILL|HUB_COMFY|HUB_ASSISTANT`
= `fixture|live`, `HUB_PUBLISHER` = `fixture|live|disabled`. A live publisher
is refused unless Paperclip is live.

## Checks

```sh
HUB_TEST_DATABASE_URL="$DATABASE_URL" cargo test --workspace   # journey tests skip without it
cargo clippy --workspace --all-targets -- -D warnings
treefmt --fail-on-change
nix build .#checks.x86_64-linux.module-eval-fixture .#checks.x86_64-linux.module-eval-live
```

Last run (2026-11-10, HEAD after review): contracts 1, domain 6, integrations
36 (+1 ignored live ComfyUI probe), journey 16, desktop 12 — all pass; clippy
and treefmt clean; both module-eval checks build. The journey tests cover the
failure modes in the brief: changed content cannot reuse an approval, stale and
duplicate timers, early timer / pause / unverifiable approval fail closed,
partial multi-account failure, lost publish response reconciled (never
resubmitted), accepted tracked to published, lost generation receipt found by
the hub-chosen prompt id, targeted cancel, human takeover beats a queued agent
reply, assets survive engine history cleanup, chat suggestions stay proposals,
GPU admission, cancel while awaiting admission, unreachable publisher retry.

Not run: Nix package builds of the desktop and server (`nix build .#…`), the
flake's clippy/nextest/fmt checks (evaluated only), any live integration.

## Capability levels

| Area | Built | Fixture-tested | Contract-verified | Live-verified |
| --- | --- | --- | --- | --- |
| Hub API, domain, worker, outbox, audit | yes | yes (journey) | — | — |
| Paperclip issues + native approvals | yes | yes | OpenAPI 0.3.1 | health/OpenAPI reads only |
| Windmill procedures + publication clock | yes | yes (in-process fixture) | OpenAPI CE 1.839.0 | `GET /api/version` only |
| ComfyUI submit/history/import/cancel | yes | yes | deployed 0.37.0 source | `/system_stats`, `/queue` reads |
| GPU admission (llama-swap `/running`) | yes | yes | response shape | — |
| Postiz publisher | yes | yes (fixture publisher) | no (public docs only) | no (not deployed) |
| n8n news ingest | adapter + `/v1/ingest/news` | ingest route yes | no | `/healthz` only |
| Local LLM assistant | yes | fixture assistant | OpenAI-compatible shape | — |
| Desktop (Dioxus 0.8.0-alpha.3) | yes | against fixture server, screenshots 1440/760 px | — | — |
| NixOS module | yes | eval checks | — | not activated anywhere |

## Live blockers (exact)

1. **ComfyUI has no checkpoints.** The atlas instance is the TRELLIS
   workbench; `poster-txt2img` needs a checkpoint. `registry/comfyui/swatch.v1.json`
   is model-free and is the only workflow that could run today.
2. **Paperclip:** no authenticated call made; the board key
   (`paperclip-operations-operator-key`) was not read. Company id unknown —
   set `HUB_COMPANY_ID`. Approval create has no idempotency key in 0.3.1: an
   ambiguous create is dead-lettered and the operator links the approval with
   `link-approval` (hash-checked).
3. **Windmill:** procedures not deployed (`wmill sync push` not run); secret
   variable `f/infernix_hub/hub_token` not created; workspace token not read.
4. **Postiz:** not deployed. When it is, the adapter refuses media that carries
   alt text (the documented API has no alt field) — confirm an alt-text field
   before enabling image posts. Lookup is unsupported, so accepted/ambiguous
   posts are resolved by an operator (`POST /v1/intents/{id}/resolve`).
5. **n8n:** no `X-N8N-API-KEY`; the `X-Studio-Token` webhook secret was not
   read; no news workflow points at `/v1/ingest/news` yet.
6. **Hub service:** the module is not imported by any host; activation,
   tokens (`infernix-hubd token`) and agenix wiring are operator steps.

No schedule was activated, nothing was published, and no engagement message
was sent.

## Review

Four Sol review rounds (`openai/gpt-6.1-sol`, xhigh) on the whole tree, the
fourth for concurrency follow-ups; round 4 approved. Round 1 found 14 issues
(3 blockers); all fixed or narrowed in `1ce7dcc`, `3b78006` and `5d6b3f1`. Known ceiling: the worker holds the outbox row lock across a
remote call; a crash between a non-idempotent call and its record can still
replay it (approvals and generations check their recorded effect first). A
lease column is the upgrade path.

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

`design/infernix-hub.fig` (OpenPencil): D01 Studio, D02 Today, M01 narrow
(390 px). Checked against screenshots of the running app at 1440 and 760 px;
layout and tokens match. The OpenPencil frames were not updated for P1–P6. 390 px was not
captured from the app (desktop window minimum in practice is wider).

## Open decisions for the owner

- Postiz vs another transport for publishing.
- Which ComfyUI checkpoint/workflow to register first.
