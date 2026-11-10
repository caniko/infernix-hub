# Nix packaging and NixOS module

`fleetix-hub` is packaged with [harbor-rs](https://github.com/caniko/harbor-rs)
toolchain, build-cache, dev-shell, and formatter helpers, and its systemd unit
is paired with [harbor-db](https://github.com/caniko/harbor-db) for database
migrations. The NixOS module also reads the
[infernix](https://github.com/caniko/infernix) option tree when a host imports
it, so ComfyUI, llama-swap, and the load balancer are wired without copying
addresses or unit names into this repository.

## Flake inputs

| Input | Pin | Why |
| --- | --- | --- |
| `nixpkgs` | `nixpkgs-unstable` | One package set for the hub, harbor-rs, harbor-db, and infernix. |
| `harbor-rs` | `6055d7d2b3a05d7d91d50938c7fb6f862a5eec85` (trunk) | `mkToolchain`, `mkCross`, `mkBuildCachePolicy`/`withRustCache`, `mkDevShells`, `mkProjectCliShellTools`, `hardeningProfiles`, `treefmtModules.rust`, and the fleet `sccache` package. |
| `harbor-db` | `b00867f501ae43a1ad4fe23fb72476ccfd10f049` | `nixosModules.default` and `services.harbor-db.projects.<name>` (migration unit, read-only check, Postgres grants). |
| `infernix` | trunk `7e9f2ad435cc0d927c38225ff634d4c8dca32554` at lock time | `nixosModules.default` for ComfyUI, llama-swap, load balancer, and GPU options. Only evaluated on `x86_64-linux`. |
| `rust-overlay`, `crane` | follow `harbor-rs` | The toolchain and cache wrapper must match the pinned harbor-rs revision. |
| `treefmt-nix` | direct, follows `nixpkgs` | harbor-rs keeps it in its dev partition; harbor-db follows this one. |

`harbor-rs`, `harbor-db`, and `infernix` all follow this flake's `nixpkgs`;
infernix also follows this flake's `harbor-rs` so the sccache namespace and the
Rust toolchain are shared.

## Packages

| Output | Binary | Notes |
| --- | --- | --- |
| `fleetix-hubd` (also `default`) | `fleetix-hubd` | Server + embedded worker. Ships `registry/` at `$out/share/fleetix-hub/registry`; the module points `HUB_REGISTRY_DIR` there. |
| `hubctl` | `hubctl` | Operator CLI over the typed API client. |
| `fleetix-hub-desktop` | `fleetix-hub-desktop` | Dioxus 0.8 desktop shell. Built separately from the server so WebKitGTK, GTK3, and libsoup-3 never enter the service closure. |

Builds go through `harbor-rs.lib.mkBuildCachePolicy` with
`withRustCache`; the wrapper and namespace come from the shared fleet policy
(`canix-rust-v5-sccache-*`), so hub builds share compiler-cache hits with the
other Rust projects on the host.

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

Formatting is treefmt only (`rustfmt` with edition 2024 and the repo's
`rustfmt.toml` max width, `alejandra` for Nix). Enter a shell and run
`treefmt <paths>`; the flake's `formatter` output is the same wrapper.

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

- `package`, `bind`, `publicUrl`, `companyId`, `assetDir`, `seedFile`,
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

### Database and migrations

The module imports `harbor-db.nixosModules.default` and registers
`services.harbor-db.projects.fleetix-hub`:

- apply command `fleetix-hubd migrate`, check command
  `fleetix-hubd migrate --check`,
- generated units `harbor-db-fleetix-hub.service` (before and required by
  `fleetix-hub.service`) and `harbor-db-fleetix-hub-check.service`,
- `HUB_MIGRATE_ON_START=0` on the runtime unit, so the server refuses to start
  when the schema is stale instead of racing migrations.

Local PostgreSQL (`database.createLocally = true`, the default) creates the
database and a role owner. When `database.migrationsUser` names a different
system user, that role owns the schema, and `postgresql-setup.service` grants
the runtime role table, sequence, and default privileges (the same shape as
pink-raven).

`database.urlFile` points at a file containing an external `DATABASE_URL`
(typically an agenix secret). A private launcher reads it at exec time, so the
URL never enters the Nix store; it implies `database.createLocally = false`.

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
- n8n: the hub starts after `n8n.service` when the service exists.
- Paperclip: `PAPERCLIP_URL` defaults to `http://127.0.0.1:3117`. Paperclip is
  a home-manager user service (`paperclip-operations`), so there is no system
  unit ordering.

### Seed, personas, accounts

When `seedFile` is null the module renders `HUB_SEED_FILE` from `engines`,
`personas`, and `accounts` in the same shape as `registry/hub.atlas.json`:

```json
{
  "engines": [ { "id": "...", "base_url": "...", "capabilities": { } } ],
  "personas": [ { "name": "...", "voice": "...", "rules": [], "avoid": [], "world": [] } ],
  "accounts": [ { "id": "...", "platform": "...", "handle": "...", "transport": "...", "persona": "...", "capabilities": { "publish": true, "comments": true, "mentions": true, "replies": true, "dms": false, "analytics": false } } ]
}
```

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

### Assertions

- fixture Windmill requires fixture ComfyUI (the in-process procedure needs
  the simulated engine),
- live Paperclip/Windmill/publisher require their credential files,
- a live publisher requires live Paperclip (fixture approvals never authorize
  real posts); only the publisher accepts `disabled`,
- `companyId` is required unless every integration is fixture,
- a live assistant requires a resolvable endpoint,
- `database.urlFile` requires `database.createLocally = false`.

## Atlas example — not activated

This is an example configuration only. It is not wired into any host; it shows
how the options line up with the current atlas services and fleet secrets.

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
