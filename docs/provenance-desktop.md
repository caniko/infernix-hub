# Desktop provenance — infernix-hub console

The desktop app (`apps/desktop`) is an adaptation of the pinned donor snapshot
of **aivyx-pa** (MIT), commit `8097870bdad7c1a726448eeeeb82331027f1e6cf`, at
`/data/nvme0/can/ProjectState/donors/aivyx-pa/src/aivyx-pa-8097870bdad7c1a726448eeeeb82331027f1e6cf`.
Only the donor's `crates/aivyx-web` shell, component kit and stylesheet were
used. The donor tree was read-only and is not part of the build graph.

The donor's `TRADEMARK.md` was read. No Aivyx name, logo, wordmark, favicon or
"Stitch" branding is copied or displayed: the app is branded "infernix hub".
Fonts (OFL) and the generic line icons are copied, as permitted.

## Donor files

| Donor file | Lines | sha256 | Destination | Modifications |
|---|---|---|---|---|
| `crates/aivyx-web/src/main.rs` | 225–228 (`main`, `dioxus::launch`) | `9b505bff042125ee81ac9b9b185206a9f38a296feb1e9250be31fb8a31b6eaec` | `apps/desktop/src/main.rs` | `console_error_panic_hook` removed; launch kept. |
| same | 251–364 (`App`, view dispatch, polling loop) | same | `apps/desktop/src/main.rs` | WebSocket coroutine + `FrontendMessage` queries replaced by `HubClient` polling; `View` replaced by six hub `Workspace`s; theme toggle and onboarding removed. |
| same | 370–403 (`Sidebar`), 405–415 (`NavItem`) | same | `apps/desktop/src/shell.rs` | Donor logo/wordmark removed; hub workspaces and 1–6 shortcuts; mobile drawer state added. |
| same | 428–446 (`Topbar`) | same | `apps/desktop/src/shell.rs` | Theme toggle and classic-UI link removed; inspector toggle and command-palette trigger added; connection label comes from `/v1/status`. |
| same | 448–460 (`StatusBar`) | same | `apps/desktop/src/shell.rs` | Segments become hub status + per-integration mode/verification; fixture mode uses a persistent banner. |
| same | 514–528 (`StatCard`) | same | `apps/desktop/src/components/mod.rs` | Icon prop replaced by a required text label plus optional detail; hub palette classes. |
| same | 544–562 (`AuditFeed`) | same | `apps/desktop/src/components/history.rs` | Audit-entry shape replaced by `HistoryLine` fed from `HistoryEntry`; newest-first ordering preserved. |
| same | 614–630 (`rel_time`) | same | `apps/desktop/src/format.rs` | `js_sys::Date::now` replaced by an injected `now_ms`, so the function is pure and unit-tested. |
| same | 738–802 (`ChatPanel`, transcript line classes) | same | `apps/desktop/src/components/chat.rs` | WebSocket sender removed; `on_send`/`on_review` events; proposal-link button added; role/class mapping preserved. |
| same | 804–831 (`GatePrompt`) | same | `apps/desktop/src/components/mod.rs` | Generic approval controls (label + approve/reject callbacks); used for reply-draft approval. |
| same | 2308–2426 (`ProposalMode`, `ProposalCard`) | same | `apps/desktop/src/components/proposal.rs` | Persona-proposal IPC replaced by display props + `on_accept(Option<edited>)` / `on_reject(Option<reason>)`; "Accept" wording for hub canon. |
| same | 2430–2463 (`DeltaRow`) | same | `apps/desktop/src/components/proposal.rs` | Delta payload replaced by display fields; the two-click confirm is reused for irreversible cancels (canon has no revert endpoint). |
| same | 2465–2514 (`ListEditor`) | same | `apps/desktop/src/components/proposal.rs` | Kept add/remove semantics; input label/id and Enter-to-add added; used for persona rules/avoid/world before submitting a canon proposal. |
| `crates/aivyx-web/assets/stitch.css` | 1–674 | `be9e8b6e732c48d5aed46090b633490604a3a28282c0063ddcf67dce55ebc1a2` | `apps/desktop/src/theme.css` | Re-palettized to the hub tokens (bg `#000000`, panel `#181714`, card `#24221d`, border `#353128`, text `#ebdbb2`, text-2 `#bdae93`, honey `#e5a72b`, focus `#f3c04f`); donor brand/light theme/mission/team/memory/document/voice styles removed; stage/context/inspector/palette/fixture-banner/asset-grid/calendar/thread styles added; focus-visible and reduced-motion rules retained. |

## Assets

| Donor file | sha256 | Destination | Notes |
|---|---|---|---|
| `assets/fonts/inter-var.woff2` | `3100e775e8616cd2611beecfa23a4263d7037586789b43f035236a2e6fbd4c62` | `apps/desktop/assets/fonts/` | OFL; inlined as a `data:` URI at startup (`theme.rs`). |
| `assets/fonts/space-grotesk-var.woff2` | `0640890476fc1198ab4de571fb658de443c4d85b66466ec09534a8737ab1ce9d` | `apps/desktop/assets/fonts/` | OFL; inlined as a `data:` URI. |
| `assets/fonts/jetbrains-mono-var.woff2` | `83c005d49d8a6a50474c73a5a36ac0468076e9c4a29da7bdb14995d80560a5be` | `apps/desktop/assets/fonts/` | OFL; inlined as a `data:` URI. |
| `assets/icons/command-center.svg` | `243aa535c99dab34e781969ac5fe89b10733ac0ed04c4a89b3205b2e0a93bbde` | `apps/desktop/assets/icons/` | Generic grid icon → Today. |
| `assets/icons/documents.svg` | `9006095e5b8f4c9c4903e4ed0856f2de062812e2b5100ad22d08d24bf454167a` | `apps/desktop/assets/icons/` | Generic document icon → News. |
| `assets/icons/missions.svg` | `ea6125461fced2c2ebeb91de3ca6d0e2632247675a8959380433fb1eb39c54b0` | `apps/desktop/assets/icons/` | Generic rocket icon → Studio. |
| `assets/icons/channels.svg` | `af10b52aa7656706b4f0eb0855f8e50d4859978c5d6b8013b3204ca846257264` | `apps/desktop/assets/icons/` | Generic broadcast icon → Calendar. |
| `assets/icons/chat.svg` | `24bdd7098da7bae1296f430df3d4e79102205f2e5844a6e4020b9ae04d7a6177` | `apps/desktop/assets/icons/` | Generic speech bubble → Relationships. |
| `assets/icons/memory.svg` | `31af06ed424ff8bf20074c7c2aa9b026e34f3ca657cfa7eef547542f9f44d085` | `apps/desktop/assets/icons/` | Generic brain icon → Library. |
| `assets/icons/agents.svg` | `1045ccc12d56841d28edcffd2abadd1b7de96d63acae847a7f8b960d47731318` | `apps/desktop/assets/icons/` | Generic robot icon, reserved for persona rows. |
| `assets/icons/settings.svg` | `541f946650649f5d87550ba26aa24962d79f7513d47a5b68fbe43bf1b5b7ed80` | `apps/desktop/assets/icons/` | Generic gear icon, reserved for inspector/actions. |

All donor `assets/logos/*` files were omitted.

## Omitted donor pieces and why

- **Brand assets and copy** (`assets/logos/*`, "Aivyx", "AIVYX", "Stitch", the
  favicon and logomark): prohibited by the donor trademark policy. Replaced by
  a CSS honey mark and the "infernix hub" wordmark.
- **WebSocket transport** (`ws_task`, `FrontendMessage`, `DaemonEnvelope`,
  `gloo_net::websocket`, `gloo_timers`): the hub exposes a typed HTTP contract
  (`hub_contracts::client::HubClient`), so the desktop talks to it directly and
  polls on the donor's `POLL_INTERVAL_MS` cadence.
- **Donor views outside the shell kit**: Missions, Teams, Memory, Documents,
  Voice, Settings, Onboarding, Classic UI. The hub's own workspaces (Today,
  News, Studio, Calendar, Relationships, Library) replace them; no donor domain
  types (`TeamMissionView`, `PersonaProposalSummary`, `SettingsSnapshot`, …) are
  carried over.
- **Theme toggle / light theme**: the hub palette is a single dark operator
  theme; the donor's `[data-theme="light"]` token block is omitted.
- **`asset!()` bundling and `font_faces()`**: desktop has no `dx` bundler; CSS
  is embedded with `include_str!` and fonts with `include_bytes!` + base64 data
  URIs (`theme.rs`).
- **Persona proposal/delta IPC helpers** (`resolve_proposal_query`,
  `revert_delta_query`, seed/draft queries): replaced by canon contract types
  (`CanonProposalIn`, `CanonDecisionIn`) and `HubClient` calls. The donor's
  append-only revert has no hub endpoint; `DeltaRow`'s two-click confirm is used
  for cancel actions instead.
- **Voice/mission/document/memory graph markup and styles**: not part of the
  hub domain.

## Contract requests

_None yet._ (Milestone C records any gaps found against `crates/contracts`.)

## Verification

- `nix develop -c cargo build -p infernix-hub-desktop`
- `nix develop -c cargo test -p infernix-hub-desktop`
