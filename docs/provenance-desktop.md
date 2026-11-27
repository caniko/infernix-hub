# Desktop provenance — fleetix-hub console

The desktop app (`apps/desktop`) is an adaptation of the pinned donor snapshot
of **aivyx-pa** (MIT), commit `8097870bdad7c1a726448eeeeb82331027f1e6cf`, at
`/data/nvme0/can/ProjectState/donors/aivyx-pa/src/aivyx-pa-8097870bdad7c1a726448eeeeb82331027f1e6cf`.
The desktop uses only the donor's `crates/aivyx-web` shell, component kit,
and stylesheet. The donor tree was read-only and stays outside the build graph.

The donor's `TRADEMARK.md` was checked. The "fleetix hub" app neither copies
nor displays the Aivyx name, logo, wordmark, favicon, or "Stitch" branding.
It copies permitted assets: OFL fonts and generic line icons.

## Donor files

| Donor file | Lines | sha256 | Destination | Modifications |
|---|---|---|---|---|
| `crates/aivyx-web/src/main.rs` | 225–228 (`main`, `dioxus::launch`) | `9b505bff042125ee81ac9b9b185206a9f38a296feb1e9250be31fb8a31b6eaec` | `apps/desktop/src/main.rs` | `console_error_panic_hook` removed; launch kept. |
| same | 251–364 (`App`, view dispatch, polling loop) | same | `apps/desktop/src/main.rs` | WebSocket coroutine + `FrontendMessage` queries replaced by `HubClient` polling; `View` replaced by ten hub `Workspace`s (four fleet, six studio); theme toggle and onboarding removed. |
| same | 370–403 (`Sidebar`), 405–415 (`NavItem`) | same | `apps/desktop/src/shell.rs` | Donor logo/wordmark removed; FLEET/STUDIO sections, count badges, hub workspaces and 1–9/0 shortcuts; mobile drawer state added. |
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
  a CSS honey mark and the "fleetix hub" wordmark.
- **WebSocket transport** (`ws_task`, `FrontendMessage`, `DaemonEnvelope`,
  `gloo_net::websocket`, `gloo_timers`): the hub exposes a typed HTTP contract
  (`hub_contracts::client::HubClient`). The desktop polls it directly on the
  donor's `POLL_INTERVAL_MS` cadence.
- **Donor views outside the shell kit**: Missions, Teams, Memory, Documents,
  Voice, Settings, Onboarding, Classic UI. The hub's own workspaces (Today,
  News, Studio, Calendar, Relationships, Library) replace them; no donor domain
  types (`TeamMissionView`, `PersonaProposalSummary`, `SettingsSnapshot`, …) are
  carried over.
- **Theme toggle / light theme**: the hub uses one dark operator theme without
  the donor's `[data-theme="light"]` token block.
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

## Runtime configuration

| Variable | Default | Purpose |
|---|---|---|
| `HUB_URL` | `http://127.0.0.1:8470` | Hub base URL; the client appends `/v1`. |
| `HUB_TOKEN` | empty | Operator bearer token. Set it when the hub enforces auth. |
| `HUB_THEME_FILE` | `$XDG_CONFIG_HOME/fleetix-hub/theme.json` (or `~/.config/…`) | Optional JSON palette override; see below. |

### Theme palette file

The console reads the palette once per process at startup:
`HUB_THEME_FILE`, else `$XDG_CONFIG_HOME/fleetix-hub/theme.json`, else
`~/.config/fleetix-hub/theme.json`. All keys are optional; the console drops
values that aren't `#rrggbb` strings:

| Key | CSS variable |
|---|---|
| `bg` | `--bg` |
| `panel` | `--panel` |
| `card` | `--card` |
| `elevated` | `--elevated` |
| `border` | `--border` |
| `text` | `--text` |
| `textMuted` | `--text-2` |
| `accent` | `--honey` |
| `focus` | `--focus` |
| `ok` / `warn` / `bad` / `info` | `--ok` / `--warn` / `--bad` / `--info` |

```json
{"bg": "#f7f4ec", "panel": "#efe9dc", "card": "#ffffff", "text": "#1d1b16", "accent": "#a86f00"}
```

The console validates operator-supplied CSS values as `#` plus six hex digits
before appending a second `:root{…}` block to the built-in stylesheet. Only
provided keys change. A missing or malformed file logs one line and keeps the
built-in dark palette without failing startup.

Text drawn on an accent (selection, primary/sage buttons, the fixture banner)
uses `var(--bg)`, so a light palette with a dark accent stays readable. Left
as-is: the built-in `:root` palette, the `rgba(...)` tints/shadows (pale
translucent accents over any panel), and `color-scheme: dark` on native
`<select>` widgets — a light palette still gets dark native select popups.

The console polls `/v1/status`, base lists, and selected detail at the donor's
1.5s cadence. It refreshes the fleet snapshot every 30s or via the palette's
"Refresh fleet status". On refresh failure it keeps the last good data and
shows the `ApiError` code/message. All lists have loading, error, and empty
states. All four fleet views share the `fleet_status_unconfigured` empty state.

## Workspace → endpoint coverage

| Workspace | Reads | Writes |
|---|---|---|
| Command Center | `GET /v1/fleet/status`, `GET /v1/today` | — |
| Hosts | `GET /v1/fleet/status` | — |
| Alerts | `GET /v1/fleet/status` | — |
| Approvals | `GET /v1/fleet/status` | — |
| Today | `GET /v1/today` | — |
| News | `GET /v1/news`, `GET /v1/personas` | `POST /v1/content` (brief) |
| Studio | `GET /v1/content`, `GET /v1/content/{id}`, `GET /v1/workflows`, `GET /v1/accounts`, `GET /v1/assets/{id}/content` | `POST /v1/content/{id}/generations`, `POST /v1/generations/{id}/cancel`, `POST /v1/assets/{id}/select`, `POST /v1/content/{id}/releases`, `POST /v1/releases/{id}/request-review`, `POST /v1/releases/{id}/schedule`, `POST /v1/intents/{id}/cancel` |
| Calendar | `GET /v1/intents`, `GET /v1/accounts` (+ bounded `GET /v1/content/{id}` release index) | `POST /v1/intents/{id}/cancel` |
| Relationships | `GET /v1/relationships`, `GET /v1/relationships/{id}` | `POST /v1/relationships/{id}/drafts`, `POST /v1/drafts/{id}/decision`, `POST /v1/relationships/{id}/takeover`, `POST /v1/relationships/{id}/release`, `POST /v1/relationships/{id}/handoff`, `POST /v1/relationships/{id}/follow-up` |
| Library | `GET /v1/personas`, `GET /v1/personas/{id}/canon`, `GET /v1/assets` | `POST /v1/personas/{id}/canon`, `POST /v1/canon/{revision}/decision` |
| Chat (content/persona) | `GET /v1/chat/{subject}/{id}` | `POST /v1/chat/{subject}/{id}` |
| Palette | `GET /v1/search?q=` | — |
| Status | `GET /v1/status` | — |

## Contract requests

1. **Intent approval state.** `PublicationIntent` exposes delivery state,
   attempts, and `release_revision_id`, but not release approval or
   `content_item_id`. Calendar indexes releases via `GET /v1/content/{id}`
   (20 content details per poll, plus a "Refresh index" button). Add
   `release_state` (preferably also `content_item_id`) to the intent payload
   or provide a release-by-id read.
2. **Release / generation by id.** `TodayItem` and palette hits can carry
   `release` or `generation` subject IDs. Without `GET /v1/releases/{id}` or
   `GET /v1/generations/{id}`, the UI opens Studio with a pointer notice,
   not the item. Add a by-ID read or `content_item_id` on the subject.
3. **Follow-up helper.** Without a `follow_up` method on
   `hub_contracts::client::HubClient`, the UI uses the public generic `post`
   to `/relationships/{id}/follow-up` with `FollowUpIn`. Confirm the route
   spelling and add a typed helper shared by CLI and desktop.
4. **Release list.** A release list or `GET /v1/releases?content_item_id=…`
   would replace Calendar's index.
5. **Incremental reads.** The console polls every base list at the donor's
   1.5s cadence. Local-first hubs are small, but `ETag`/`updated_since` would
   keep polling cheap as content grows.

## Known gaps

- The desktop builds/tests and smoke run passed (Weston headless,
  mock hub). The compositor had no screen-capture protocol, so no screenshot
  was captured. A manual visual check in a real desktop session is still
  recommended.
- Desktop milestone commits omit the workspace-owned `Cargo.lock`. The owner
  must lock the new dependencies (`contracts/client`, `chrono`, `serde`,
  `serde_json`, `tokio`, `uuid`) before `cargo build --locked` or the Nix
  desktop package can succeed.
- `datetime-local` fields use UTC and carry a `(UTC)` label; the hub stores
  UTC instants.
- `HUB_TOKEN` is read from the environment; there is no login UI.
- The webview may block `window.open` for handoff/posts. The UI always shows
  the returned URL; reopen it from the button.

## Toolchain

The app pins **Dioxus `=0.8.0-alpha.3`** (the newest 0.8 alpha on crates.io on
2026-11-09) with the `desktop` feature. `cargo build -p fleetix-hub-desktop`
builds the binary without `dx`.

The 0.6 → 0.8-alpha migration touched one API: keyboard matching now uses
`Key::Named(NamedKey::Enter)` (the 0.6 `Key::Enter` variant is gone).
`EventHandler` (now an alias of `Callback`), `use_signal`, `use_coroutine`,
`document::Title` and `dangerous_inner_html` remain source-compatible.

## Verification

Final checks ran from the repo root in the checked-in devshell:

- `nix develop -c cargo build -p fleetix-hub-desktop` — passes (Dioxus 0.6 for
  milestone A, 0.8.0-alpha.3 for milestones B/C).
- `nix develop -c cargo test -p fleetix-hub-desktop` — 12 pure-helper tests pass.
- `nix develop -c cargo clippy -p fleetix-hub-desktop --all-targets -- -D warnings`
  — clean.
- Headless smoke run: Weston headless + a throwaway mock hub (`/v1/status`,
  today, news, content detail, relationships, canon, chat, assets). The app
  stayed up for the full timeout with no panic; only expected GTK/devtools
  warnings. No screenshot was captured because the headless compositor did not
  expose the screen-capture protocol.

During development, another workstream's untracked `nix/treefmt.nix` in the
shared checkout made `nix develop` fail flake evaluation. A scratch snapshot
of the same flake supplied the equivalent devshell until Git tracked the file.
