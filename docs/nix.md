# Nix packaging and NixOS module

`fleetix-hub` uses [harbor-rs](https://github.com/caniko/harbor-rs) for the
toolchain, build cache, dev shell, and formatter, and pairs its systemd unit
with [harbor-db](https://github.com/caniko/harbor-db) migrations. When a host
imports [infernix](https://github.com/caniko/infernix), the NixOS module reads
its option tree to wire ComfyUI, llama-swap, and the load balancer without
copying addresses or unit names here.

## Flake inputs

| Input | Pin | Why |
| --- | --- | --- |
| `nixpkgs` | `nixpkgs-unstable` | One package set for the hub, harbor-rs, harbor-db, and infernix. |
| `harbor-rs` | `6055d7d2b3a05d7d91d50938c7fb6f862a5eec85` (trunk) | `mkToolchain`, `mkCross`, `mkBuildCachePolicy`/`withRustCache`, `mkDevShells`, `mkProjectCliShellTools`, `hardeningProfiles`, `treefmtModules.rust`, and the fleet `sccache` package. |
| `harbor-db` | `b00867f501ae43a1ad4fe23fb72476ccfd10f049` | `nixosModules.default` and `services.harbor-db.projects.<name>` (migration unit, read-only check, Postgres grants). |
| `infernix` | trunk `7e9f2ad435cc0d927c38225ff634d4c8dca32554` at lock time | `nixosModules.default` for ComfyUI, llama-swap, load balancer, and GPU options. Only evaluated on `x86_64-linux`. |
| `rust-overlay`, `crane` | follow `harbor-rs` | The toolchain and cache wrapper must match the pinned harbor-rs revision. |
| `treefmt-nix` | direct, follows `nixpkgs` | harbor-rs keeps it in its dev partition; harbor-db follows this one. |

`harbor-rs`, `harbor-db`, and `infernix` follow this flake's `nixpkgs`.
`infernix` also follows this flake's `harbor-rs` to share the sccache namespace
and Rust toolchain.

## Packages

| Output | Binary | Notes |
| --- | --- | --- |
| `fleetix-hubd` (also `default`) | `fleetix-hubd` | Server + embedded worker. Ships `registry/` at `$out/share/fleetix-hub/registry`; the module points `HUB_REGISTRY_DIR` there. |
| `hubctl` | `hubctl` | Operator CLI over the typed API client. |
| `fleetix-hub-desktop` | `fleetix-hub-desktop` | Dioxus 0.8 desktop shell. Built separately from the server so WebKitGTK, GTK3, and libsoup-3 never enter the service closure. |

Builds use `harbor-rs.lib.mkBuildCachePolicy` with `withRustCache`. The fleet
policy supplies the wrapper and namespace (`canix-rust-v5-sccache-*`), so hub
builds share compiler-cache hits with other Rust projects on the host.

## Development shell

`nix develop` starts `harbor-rs.lib.mkDevShells` with:

- the desktop `pkgConfigDeps` and `LD_LIBRARY_PATH` (plus
  `WEBKIT_DISABLE_DMABUF_RENDERER=1` for GPU stacks that break WebKit's DMA-BUF
  renderer),
- PostgreSQL 16 client, `cargo-nextest`, and the treefmt wrapper,
- `hubctl` from this checkout via `mkProjectCliShellTools` (it must resolve to
  the flake build, not a stale PATH entry),
- cross shells (`windows`, `macos`, `cross`) from `mkCross`.

```console
nix develop -c treefmt flake.nix nix
nix develop -c cargo nextest run          # DB tests self-skip without HUB_TEST_DATABASE_URL
nix develop -c hubctl today               # needs HUB_URL/HUB_TOKEN
```

Use only treefmt: `rustfmt` uses edition 2024 and the `rustfmt.toml` max
width; `alejandra` formats Nix. Run `treefmt <paths>` in the dev shell.
The flake's `formatter` output uses the same wrapper.

## Scoped Codex workers

The worker listener defaults to off. Enable the local Home Manager proxy's
private Unix socket with:

```nix
programs.fleetixHub.agentProxy.workerListener = {
  enable = true;
  allowedModels = ["gpt-6.2-sol"];
};
```

The default socket is `$XDG_RUNTIME_DIR/fleetix-agent-proxy/worker.sock`.
The proxy creates a `0600` socket in a required service-owned `0700` parent
directory. Unix socket clients need a bearer only if the service starts with
a separate worker key.

For CLI use, pass `--worker-unix-socket PATH` or
`--worker-listen 127.0.0.1:PORT` to `fleetix-agent-proxy serve`, together
with `--worker-allowed-models MODEL[,MODEL...]`. TCP requires
`--worker-api-key-file PATH`: a service-owned regular file with no group or
world permissions. Generate a random bearer of at least 32 characters,
distinct from every client, management, and provider credential.

The listener accepts only `POST /backend-api/codex/responses` with an exact
model allowlist match. Workers always use a subscription credential for the
ChatGPT Codex backend, never a metered OpenAI API key. The listener exposes
neither proxy management nor model discovery.

## Checks

| Check | What it proves |
| --- | --- |
| `checks.<system>.clippy` | Native workspace clippy with `-D warnings`, desktop excluded. |
| `checks.<system>.nextest` | Native nextest run; database tests self-skip without `HUB_TEST_DATABASE_URL`. |
| `checks.<system>.fmt` | treefmt check over the flake source. |
| `checks.x86_64-linux.module-eval-fixture` | The module alone, every integration fixture: fixture company id, no seed file, `HUB_MIGRATE_ON_START=0`, local PostgreSQL, harbor-db migration + `migrate --check` units gating `fleetix-hub.service`. |
| `checks.x86_64-linux.module-eval-live` | The module plus `infernix.nixosModules.default` with ComfyUI and llama-swap enabled: seed engine `comfy-<host>` at `http://127.0.0.1:8188`, admission policy JSON, `LLM_URL` at llama-swap, unit `After`/`Wants` for both services, migration gate enabled. |

```console
nix flake show
nix flake check --no-build
nix build .#checks.x86_64-linux.module-eval-fixture
nix build .#checks.x86_64-linux.module-eval-live
```

## Module overview

`nixosModules.default` (alias `nixosModules.fleetix-hub`) exposes
`services.fleetix-hub`.

- `executable` (default `${package}/bin/fleetix-hubd`), `package`, `bind`,
  `publicUrl`, `companyId`, `assetDir`, `seedFile`, `fleetStatusFile`,
  `environment`.
- `database`: `createLocally`, `name`, `url`, `urlFile`,
  `migrationsUser`, `migrationsGroup`; `migrations.enable`.
- `integrations`: `paperclip`, `windmill`, `comfy`, `assistant`
  (`fixture` / `live`) and `publisher` (`fixture` / `live` / `disabled`).
- `assistant`: `endpoint` (`llama-swap` / `load-balancer` / `custom`), `url`,
  `model`.
- `paperclip.url`, `windmill.url`, `windmill.workspace`.
- `engines`, `personas`, `accounts`, `gpuAdmission`.
- `tokens`, `credentials`.
- `packs.<id>`: `enable`, `config`, `credentials`.
- `feeds.<id>`: `kind`, `endpoint`, `region`, `bucket`, `key`, `intervalSecs`,
  `staleAfterSecs`, `minStatus`, `credentials.accessKeyId`,
  `credentials.secretAccessKey`.

### Database and migrations

The module imports `harbor-db.nixosModules.default` and registers
`services.harbor-db.projects.fleetix-hub`:

- apply command `${package}/bin/fleetix-hubd migrate`, check command
  `fleetix-hubd migrate --check` (always the core binary, never `executable`),
- generated units `harbor-db-fleetix-hub.service` (before and required by
  `fleetix-hub.service`) and `harbor-db-fleetix-hub-check.service`,
- `HUB_MIGRATE_ON_START=0` on the runtime unit, so the server refuses to start
  when the schema is stale instead of racing migrations.

Local PostgreSQL (`database.createLocally = true`, the default) creates the
database and its owner role. If `database.migrationsUser` names a different
system user, that role owns the schema and `postgresql-setup.service` grants the
runtime role table, sequence, and default privileges.

`database.urlFile` points to an external `DATABASE_URL` file (typically an
agenix secret) and implies `database.createLocally = false`. A private
launcher reads it at exec time so the URL stays out of the Nix store.

### ComfyUI, GPU admission, and engines

When `services.infernix.comfyui.enable` is set, the module:

- defaults `integrations.comfy = "live"`,
- registers engine `comfy-<hostName>` at
  `http://<host>:<port>` (a `0.0.0.0`/`::` listener is reached over
  `127.0.0.1`),
- orders `fleetix-hub.service` after and wants `comfyui.service`.

Additional engines can be declared explicitly, keyed by id:

```nix
services.fleetix-hub.engines.murph-comfy = {
  baseUrl = "http://10.123.0.7:8190";
  capabilities = { };
};
```

Murph's ComfyUI is an on-demand (socket-activated, idle-exit) instance. Do not
add a health probe or a startup `Wants` for it; an idle probe would keep
starting the GPU workload.

Every engine's capabilities receive the admission policy:

```json
{
  "max_in_flight": 1,
  "defer_while_llm_loaded": true,
  "llm_running_url": "http://127.0.0.1:8013/running",
  "defer_on_unknown_work": true
}
```

`gpuAdmission.deferWhileLlmLoaded` defaults to llama-swap being enabled on the
same host, and `gpuAdmission.llmRunningUrl` defaults to its `/running`
endpoint. `deferOnUnknownEngineWork` defaults to `true` (fail closed).

### Assistant (llama-swap / load balancer)

`assistant.endpoint` defaults to `load-balancer` when
`services.infernix.loadBalancer.enable` (or `services.infernix.fleet.loadBalancer`
generation) is enabled on the host, otherwise `llama-swap`. The module then
sets `LLM_URL` (`.../v1`) and `LLM_MODEL` (default `qwen3.8-27b`, which is in
the atlas catalog), defaults `integrations.assistant = "live"`, and orders the
unit after/wants `llama-swap.service` or `infernix-lb.service` respectively.
`endpoint = "custom"` requires `assistant.url`.

### Windmill, n8n, Paperclip

- Windmill: when `services.windmill` is enabled (or `windmill-server.service`
  exists), `WINDMILL_URL` defaults to `http://127.0.0.1:<serverPort>` (atlas:
  8040) and the hub starts after `windmill-server.service`.
- n8n: the hub starts after `n8n.service` when the service exists. Setting
  `N8N_PUBLISH_PLATFORMS` (comma list, e.g. `youtube,bluesky,mastodon`)
  registers one publishing channel `n8n:<platform>` per entry against
  `N8N_PUBLISH_URL`, authenticated with `N8N_WEBHOOK_TOKEN_FILE` (contract v1,
  `studio-pipeline/n8n/README.md`). An account publishes through it when its
  `transport` is `n8n:<platform>`.
- Paperclip: `PAPERCLIP_URL` defaults to `http://127.0.0.1:3117`. Paperclip is
  a home-manager user service (`paperclip-operations`), so there is no system
  unit ordering. The optional `paperclip_studio` settings row
  (`{"project_id": …, "assignee_agent_id": …, "ceo_agent_id": …}`) routes delegated briefs to the
  studio's project and producer agent and canon comment requests to the CEO.

### Publishing channels and approvals

A release action uses only its frozen `transport` channel. A missing channel
fails the intent for an operator; there is no provider fallback.

Only `integrations.publisher = "fixture"` aliases the seeded account transports
to the in-process fixture adapter. A live publisher (`postiz`), every
`n8n:<platform>` channel and every pack channel posts for real, so each one
requires `integrations.paperclip = "live"`; startup fails with the channel name
when fixture approvals would be the only authority for a real post. A pack
channel may take a name that only exists as a fixture alias (`plinth` next to a
seeded `transport = "plinth"`); a collision with a live built-in or another
pack fails startup.

### Seed, personas, accounts, renewals

When `seedFile` is null the module renders `HUB_SEED_FILE` from `engines`,
`personas`, `accounts`, and `renewals` in the same shape as
`registry/hub.atlas.json`:

```json
{
  "engines": [ { "id": "...", "base_url": "...", "capabilities": { } } ],
  "personas": [ { "name": "...", "voice": "...", "rules": [], "avoid": [], "world": [] } ],
  "accounts": [ { "id": "...", "platform": "...", "handle": "...", "transport": "...", "persona": "...", "capabilities": { "publish": true, "comments": true, "mentions": true, "replies": true, "dms": false, "analytics": false } } ],
  "renewals": [ { "key": "linkedin-personal-token", "title": "LinkedIn access token", "procedure": "Re-authorize the app and update the credential.", "urls": ["https://www.linkedin.com/developers/apps"], "account": null, "cadence_days": 60, "warn_days": 14, "due_at": null, "expiry_kind": "cadence" } ]
}
```

The hub upserts `renewals` by `(company_id, key)`. New rows start with the
seed's `due_at`; re-seeding refreshes only `title`, `procedure`, `urls`,
`account`, `cadence_days`, `warn_days`, and `expiry_kind`. Restarts and
rebuilds leave the operator's due date and last renewal unchanged.
The hub rejects the seed if any `urls` entry does not start with `https://`
because they open in a browser.

`expiryKind` (`cadence`, `never` or `provider`, default `cadence`) says where a
renewal's expiry comes from. `never` is listed but never a to-do. `provider`
is judged by the hub from the collector's observations: a fresh answer (48 h)
becomes due or overdue inside `warnDays`, a missing or stale one is
`unverifiable`, and a provider-confirmed inactive credential is `invalid`.
Observations arrive through `POST /v1/renewals/{key}/observe` (operator or
procedure); a re-seed never touches them.

`company_id` stays out of the generated seed; live deployments pass
`companyId`. With every integration in `fixture`/`disabled`, the module falls
back to `fixture-company`, so a fixture-only host needs no Paperclip company.

### Tokens and credentials

`tokens.<name> = { role, file }` registers a bearer token before the server
starts:

```nix
ExecStartPre = fleetix-hubd token --name <name> --role <role> --token-file %d/token-<name>
```

with `LoadCredential=token-<name>:<file>`. Registration is idempotent, so
restarts and rebuilds do not rotate tokens.

Integration credentials (`credentials.paperclipToken`, `windmillToken`,
`postizToken`) are also `LoadCredential` entries exposed as
`PAPERCLIP_TOKEN_FILE`, `WINDMILL_TOKEN_FILE`, and `POSTIZ_TOKEN_FILE`. Point
them at agenix paths; plaintext never goes into the store.

### Executable override and packs

`executable` (default `${package}/bin/fleetix-hubd`) runs `serve`. Downstream
binaries such as `canix-hub` must accept `serve [--packs-config FILE]`.
Core token registration and the harbor-db `migrate`/`migrate --check` runner
always use `${package}/bin/fleetix-hubd`.

`packs.<id>` composes a downstream pack without editing the hub:

```nix
services.fleetix-hub.packs.plinth = {
  enable = true;
  config = {baseUrl = "https://example.com";}; # any JSON-serializable attrset
  credentials.token = config.age.secrets.plinth-token.path;
};
```

Enabled packs are rendered to `packs.json` (id -> config), so the unit starts
as

```text
<executable> serve --packs-config /nix/store/...-fleetix-hub-packs.json
```

and startup fails when that file names an id the binary does not register.
Disabled packs contribute neither config nor credentials.

Each pack credential becomes a systemd credential `<id>-<name>`. Only its
path enters the store; the pack reads the file from `$CREDENTIALS_DIRECTORY`.
Pack IDs follow the Rust registry's `[a-z0-9-]+` rule and credential names
must match `[A-Za-z0-9_.-]+` to produce valid systemd credential IDs. The
module rejects duplicate `LoadCredential` IDs across built-ins, `token-<name>`
entries, and pack credentials. Systemd silently keeps one entry per repeated ID,
and `foo` + `bar-baz` and `foo-bar` + `baz` both render `foo-bar-baz`.

### Fleet status snapshot

`fleetStatusFile` (default null) points to the `fleetix::status::FleetStatus`
JSON that `canix fleet status --write` replaces atomically every few minutes.
The module exports the path as `HUB_FLEET_STATUS_FILE`. The hub collects no
fleet data itself: `GET /v1/fleet/status` rereads the file per request or
returns `404 fleet_status_unconfigured` when unset.

With `ProtectHome` and `ProtectSystem=strict`, the unit bind-mounts the
snapshot's **parent directory** read-only (`BindReadOnlyPaths`). Atomic writes
rename a new inode into place; mounting the file itself would hide replacements.
Put the file where the writer can rename it atomically within its directory
(for example `/var/lib/fleet-status/fleet.json`).

### Notifications and the producer loop

`serve` runs one in-process producer loop next to the outbox worker and, like
the worker, not under `--no-worker`. Every 60 s it:

- syncs the `today` source from the same read behind `GET /v1/today`, with no
  caller role: pending reviews, failed operations, overdue interactions,
  renewals and opportunities become `today:*` rows and resolve when their
  bucket clears. Due releases and active production are informational and never
  notify;
- syncs the `fleet` source from `fleetStatusFile`: unhealthy hosts, pending
  approvals (`fleet:op:<id>`), monitoring errors and alerts, and a failed
  nightly run. A snapshot older than 15 minutes, or one that cannot be read,
  raises `fleet:stale`. An outdated-but-healthy host is expected most of the day
  and never notifies;
- syncs the `hub` source from summary staleness (below);
- clears lapsed snoozes, which announces the row again, and deletes rows that
  were acknowledged and resolved more than 30 days ago.

An unchanged condition leaves the row and `seq` untouched, so its
acknowledgement survives. The loop logs step failures and retries them on the
next pass; none is fatal.

`HUB_SUMMARY_STALE_HOURS` (unset by default) turns on `hub:summary-stale`: when
no `source = summary` notification has been written for that many hours, the hub
raises a warning, because the daily summary task did not run. Unset means no
staleness rule: the `hub` source syncs an empty set, which resolves any existing
raise. Atlas sets `26`, so a missed 07:30 summary shows up the next morning.

### Status feeds

`feeds.<id>` renders `HUB_FEEDS_CONFIG` (see "Status feeds" in
`capabilities.md`). The two credential paths become the systemd credentials
`feed-<id>-access-key-id` and `feed-<id>-secret-access-key`, and the generated
config names only those ids, so no key material enters the store, the config
or the environment.

```nix
services.fleetix-hub.feeds.syndb = {
  kind = "s3"; # default
  endpoint = "https://s3-west.nrp-nautilus.io";
  region = "us-east-1";
  bucket = "syndb-status";
  key = "report.json";
  intervalSecs = 300;
  staleAfterSecs = 2700;
  minStatus = "warn"; # default; unknown | warn | fail
  credentials.accessKeyId = config.age.secrets.syndb-status-read-key-id.path;
  credentials.secretAccessKey = config.age.secrets.syndb-status-read-secret.path;
};
```

### Assertions

- fixture Windmill requires fixture ComfyUI (the in-process procedure needs
  the simulated engine),
- live Paperclip/Windmill/publisher require their credential files,
- a live publisher requires live Paperclip (fixture approvals never authorize
  real posts); only the publisher accepts `disabled`,
- generated `LoadCredential` ids are unique across built-ins, tokens, packs and feeds,
- feed ids match `[a-z0-9-]{1,32}`, `staleAfterSecs >= intervalSecs`, and feed
  credentials are absolute paths outside the store,
- `companyId` is required unless every integration is fixture,
- a live assistant requires a resolvable endpoint,
- `database.urlFile` requires `database.createLocally = false`.

## Atlas example — not activated

This example maps hub options to current Atlas services and fleet secrets.
No host imports it.

```nix
{ config, inputs, ... }: {
  imports = [inputs.fleetix-hub.nixosModules.default];

  services.fleetix-hub = {
    enable = true;
    bind = "127.0.0.1:8470";
    # Paperclip company id from the board; the hub refuses to start without it
    # when any integration is live.
    companyId = "cmp_atlas";

    database = {
      createLocally = true;
      # Migrations run as an operator peer role; the fleetix-hub role receives
      # grants and stays unable to run DDL.
      migrationsUser = "can";
    };

    integrations = {
      comfy = "live";        # from services.infernix.comfyui
      assistant = "live";    # from services.infernix.llama-swap
      windmill = "live";     # from services.windmill (studio workspace)
      paperclip = "live";    # paperclip-operations user service, :3117
      publisher = "disabled"; # Postiz is not deployed
    };

    assistant = {
      # Local llama-swap is deliberate: qwen3.8-27b is the coding profile on
      # this host. The default would pick the load balancer, which is also
      # enabled on atlas.
      endpoint = "llama-swap";
      model = "qwen3.8-27b";
    };

    credentials = {
      paperclipToken = config.age.secrets.paperclip-operations-operator-key.path;
      windmillToken = config.age.secrets.paperclip-studio.path;
    };

    tokens = {
      operator.file = config.age.secrets.fleetix-hub-operator-token.path;
      windmill-procedure.file = config.age.secrets.fleetix-hub-windmill-token.path;
      n8n-ingest.file = config.age.secrets.fleetix-hub-n8n-token.path;
    };

    personas = [
      {
        name = "Juniper Vale";
        voice = "Warm, precise, a little wry. Explains why a thing matters before how it works.";
        rules = ["Credit sources by name" "One idea per post"];
        avoid = ["Hype words" "Unverified claims"];
        world = ["Juniper runs a small studio on a hillside"];
      }
    ];

    accounts = [
      {
        id = "atlas-mastodon";
        platform = "mastodon";
        handle = "@juniper@example.social";
        transport = "fixture"; # Postiz is not deployed yet
        persona = "Juniper Vale";
        capabilities.analytics = false;
      }
    ];

    # Defaults: one engine job in flight, defer while qwen3.8-27b is resident,
    # fail closed on unknown engine load.
    gpuAdmission = { };

    # ComfyUI/llama-swap/windmill ordering and URLs are derived from the
    # infernix and windmill option trees already imported on atlas.
  };
}
```

Notes for the example:

- `services.infernix.comfyui` on atlas is loopback on `127.0.0.1:8188`; the
  generated engine id is `comfy-atlas`.
- `services.infernix.llama-swap` is on `:8013`; `HUB_ASSISTANT` defaults to
  `live` and `LLM_URL` becomes `http://127.0.0.1:8013/v1`.
- Windmill units are `windmill-server.service` on `:8040`; the workspace is
  `studio`.
- `paperclip-operations-operator-key` and `paperclip-studio` are the fleet
  secret names; wire them through `age.secrets` on the host before enabling.
- No Postiz deployment exists, so the publisher is `disabled` (a `fixture`
  publisher would silently simulate publishing).

## Secret inventory

Set `services.fleetix-hub.secretInventoryFile` to an absolute inventory JSON
path outside `/home`, `/root`, and `/run/user` that the service user can read.
The read-only directory mount keeps atomic replacements visible. An unreadable
inventory preserves previous secret alerts and raises `secrets:inventory-stale`.
Readable snapshots older than 25 hours retain their alerts and also raise staleness.
