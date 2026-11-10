# Source provenance

infernix-hub is an independent product with its own history, licensed under
the EUPL-1.2 (`LICENSE`). It reuses
selected code from one pinned historical snapshot of Aivyx PA. Reuse does
not change its nature: the reused parts are MIT-licensed code from that
snapshot, adapted as recorded here and in the two detail files.

## Donor record

| Item | Value |
| --- | --- |
| Repository | https://github.com/Aivyx-Agent/aivyx-pa |
| Commit | `8097870bdad7c1a726448eeeeb82331027f1e6cf` (pinned; not `main`, not a tag) |
| Archive | https://github.com/Aivyx-Agent/aivyx-pa/archive/8097870bdad7c1a726448eeeeb82331027f1e6cf.tar.gz |
| Archive SHA-256 | `24f3afd9d5ba96ed32fb76e7a90f6e839c1f73a2f371b82f37bdfe6a28c50c2f` (4,639,682 bytes) |
| Retrieved | 2026-11-09T21:39:02Z, on host `atlas`, with `curl` (no GitHub fork, no hosted repo) |
| Unpacked, read-only | `/data/nvme0/can/ProjectState/donors/aivyx-pa/src/aivyx-pa-8097870bdad7c1a726448eeeeb82331027f1e6cf` (outside this repo and its build graph) |
| License | MIT, `Copyright (c) 2026 Julian (Aivyx)` — notice kept in `LICENSE-THIRD-PARTY.md` |
| Trademark | Donor `TRADEMARK.md` read: the Aivyx name, logos and branding are not used |

Rechecked after retrieval: the snapshot contains `crates/aivyx-web`, a Dioxus
`0.6` web client (`dioxus = { version = "0.6", features = ["web"] }`) that
talks to the daemon over a WebSocket through `aivyx-ipc`. It contains no
ComfyUI gallery; the hub's production studio is built from the ComfyUI API
contracts (`docs/capabilities.md`).

Donor project policies, scripts (`scripts/dev-run.sh`, `install-hooks.sh`,
`pre-commit.sh`), installers, Docker deploy, `dist` release config and
publishing workflows were read as context only. None was run or adopted.

## Dependency graph inspected

| Donor crate | Internal deps | Decision |
| --- | --- | --- |
| `aivyx-web` | `aivyx-ipc` (+ dioxus 0.6 web, gloo-net, gloo-timers, web-sys, js-sys) | **adapt**: shell, component kit, stylesheet, fonts, generic icons |
| `aivyx-ipc` | `aivyx-team-types` → `aivyx-capability` | **adapt pattern only**: one dependency-light wire crate shared by every client; message types replaced |
| `aivyx-n8n` | `aivyx-tool`, `aivyx-core`, `aivyx-capability`, `aivyx-auth-cli` | **adapt `n8n_client.rs` only**; the crate's tool/agent plumbing is not imported |
| `aivyx-mcp` | `aivyx-core`, `aivyx-capability` | **omit**: the hub exposes scoped HTTP tools to agents; no MCP transport is needed yet |
| `aivyx-tui` | `aivyx-channel`, `aivyx-core` | **defer**: terminal views come after the desktop journey (`hubctl` covers CLI use) |
| `aivyx-team`, `aivyx-team-types` | — | **replace**: Paperclip owns agents, assignments and approvals |
| loop / scheduling in the daemon | — | **replace**: Paperclip routines and Windmill procedures; Windmill owns the publication clock |
| `aivyx-storage` (redb) | — | **replace**: PostgreSQL owns hub metadata |
| daemon `/ws` + Unix socket | — | **replace**: Axum API + typed `HubClient` |
| `aivyx-voice`, `aivyx-kitchen`, gmail/calendar/drive/contacts/notion/obsidian/telegram/discord/slack, federation | — | **omit**: out of product scope |

## Origin map (retained behaviour)

| Hub location | Donor origin | Retained behaviour |
| --- | --- | --- |
| `apps/desktop/src/shell.rs` | `aivyx-web/src/main.rs` 370–460 | Sidebar navigation, top bar with live connection state, status bar segments |
| `apps/desktop/src/main.rs` | `main.rs` 225–364 | App root, context-provided state, fixed-interval refresh loop (`POLL_INTERVAL_MS`) |
| `apps/desktop/src/components/chat.rs` | `main.rs` 738–802 | Transcript with role classes, Enter-to-send composer, disabled-until-ready input |
| `apps/desktop/src/components/proposal.rs` | `main.rs` 2308–2514 | Review card modes (view / edit-and-accept / reject-with-reason), two-click confirm row, list editor |
| `apps/desktop/src/components/mod.rs` | `main.rs` 514–528, 804–831 | Stat cards; approve/reject gate controls |
| `apps/desktop/src/components/history.rs` | `main.rs` 544–562 | Newest-first audit feed |
| `apps/desktop/src/format.rs` | `main.rs` 614–630 | Relative-time labels (made pure and tested) |
| `apps/desktop/src/theme.css` | `aivyx-web/assets/stitch.css` | Token/component stylesheet, focus-visible and reduced-motion rules (re-palettized) |
| `apps/desktop/assets/fonts/*`, `assets/icons/*` | `aivyx-web/assets/...` | OFL fonts, generic line icons |
| `crates/contracts` | `aivyx-ipc` (crate shape) | Single wire-truth crate with no async/storage deps, shared by server and every client |
| `crates/integrations/src/live/n8n.rs` | `aivyx-n8n/src/n8n_client.rs` (sha256 `2f012e59…f97`) | Base-URL trimming, `/api/v1` joins, `X-N8N-API-KEY`, response decoding, error enum, tests — with secrets redacted from `Debug` and a production-webhook trigger added |

Canon proposals in the hub follow the donor's persona-governance idea
(`aivyx-ipc/src/persona.rs`, proposals gated by the operator, append-only
deltas). No code from that file was copied; the hub stores canon as
append-only revisions in PostgreSQL.

## Detail files

- Desktop: [`provenance-desktop.md`](provenance-desktop.md) — every donor
  region, file hash, destination and modification.
- Integrations: [`provenance-integrations.md`](provenance-integrations.md) —
  the n8n client adaptation plus the third-party API contracts captured.

## Changing the donor

Later upstream changes are imported only after a deliberate review and a new
row here naming the commit, archive hash and the files taken.
