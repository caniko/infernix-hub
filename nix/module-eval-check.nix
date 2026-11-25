# Cheap eval-only checks for the NixOS module. Two systems are evaluated: the
# hub alone (all fixtures) and the hub next to the real infernix ComfyUI +
# llama-swap modules. Each check builds only a runCommand.
{
  pkgs,
  module,
  infernixModule,
}: let
  inherit (pkgs) lib;
  system = pkgs.stdenv.hostPlatform.system;

  mkSystem = modules:
    import (pkgs.path + "/nixos/lib/eval-config.nix") {
      inherit system;
      inherit modules;
    };

  fixture = mkSystem [
    module
    ({config, ...}: {
      system.stateVersion = config.system.nixos.release;
      networking.hostName = "evalhost";
      services.fleetix-hub.enable = true;
      # Drive the real migration launcher without building the Rust daemon or
      # connecting to a database.
      services.fleetix-hub.package = pkgs.writeShellScriptBin "fleetix-hubd" ''
        set -eu
        [[ "$*" == "migrate --check" ]]
        echo attempt >> attempts
        if [[ "$TEST_MODE" == recover && $(wc -l < attempts) == 2 ]]; then
          exit 0
        fi
        echo "$TEST_ERROR" >&2
        exit "$TEST_STATUS"
      '';
    })
  ];

  live = mkSystem [
    module
    infernixModule
    ({config, ...}: {
      system.stateVersion = config.system.nixos.release;
      networking.hostName = "evalhost";
      services.infernix.gpu = {
        vendor = "amd";
        inherit pkgs;
      };
      services.infernix.comfyui = {
        enable = true;
        # Eval only: never built, only its path is rendered into ExecStart.
        package = pkgs.hello;
      };
      services.infernix.llama-swap = {
        enable = true;
        modelsDir = "/var/lib/infernix-models";
      };
      services.fleetix-hub = {
        enable = true;
        fleetStatusFile = "/var/lib/fleet-status/fleet.json";
        secretInventoryFile = "/var/lib/secret-inventory/inventory.json";
        feeds.syndb = {
          endpoint = "https://s3-west.nrp-nautilus.io";
          region = "us-east-1";
          bucket = "syndb-status";
          key = "report.json";
          intervalSecs = 300;
          staleAfterSecs = 2700;
          credentials.accessKeyId = "/run/agenix/syndb-status-key-id";
          credentials.secretAccessKey = "/run/agenix/syndb-status-secret";
        };
        renewals = [
          {
            key = "eval-token";
            title = "Eval token";
            urls = ["https://example.org/oauth/apps"];
            expiryKind = "provider";
          }
        ];
      };
    })
  ];

  # A presigned URL pasted as a feed endpoint: the config must refuse to
  # render it, even when evaluated on its own.
  presigned = mkSystem [
    module
    ({config, ...}: {
      system.stateVersion = config.system.nixos.release;
      services.fleetix-hub = {
        enable = true;
        feeds.leak = {
          endpoint = "https://s3.example.org/b/k?X-Amz-Signature=deadbeef";
          region = "us-east-1";
          bucket = "b";
          key = "k";
          intervalSecs = 300;
          staleAfterSecs = 600;
          credentials.accessKeyId = "/run/agenix/a";
          credentials.secretAccessKey = "/run/agenix/b";
        };
      };
    })
  ];

  fixtureHub = fixture.config.systemd.services.fleetix-hub;
  fixtureMigration = fixture.config.systemd.services."harbor-db-fleetix-hub";
  fixtureCheck = fixture.config.systemd.services."harbor-db-fleetix-hub-check";
  fixtureActivation = fixture.config.systemd.services.harbor-db-fleetix-hub-activate;

  external = database:
    (fixture.extendModules {
      modules = [
        {
          services.fleetix-hub = {
            database = {createLocally = false;} // database;
            systemd.onFailure = ["hub-alert.service"];
          };
        }
      ];
    }).config.systemd.services;
  externalUrl = "postgresql://db.example.invalid/fleetix-hub";
  remote = external {url = externalUrl;};
  secret = external {urlFile = "/run/agenix/hub-db-url";};

  liveHub = live.config.systemd.services.fleetix-hub;
  # ComfyUI enabled but the hub on its fixture: an on-demand ComfyUI must not
  # be pulled in at boot by the hub.
  fixtureComfyHub =
    (live.extendModules {
      modules = [{services.fleetix-hub.integrations.comfy = "fixture";}];
    }).config.systemd.services.fleetix-hub;
  liveMigration = live.config.systemd.services."harbor-db-fleetix-hub";
  liveCheck = live.config.systemd.services."harbor-db-fleetix-hub-check";
  liveSeed = liveHub.environment.HUB_SEED_FILE;
in {
  # Hub alone, every integration fixture: fixture company, no seed, and the
  # harbor-db migration gate enabled with local PostgreSQL.
  module-eval-fixture = assert fixtureHub.environment.HUB_COMPANY_ID == "fixture-company";
  assert fixtureHub.environment.HUB_MIGRATE_ON_START == "0";
  assert !(fixtureHub.environment ? HUB_SEED_FILE);
  assert !(fixtureHub.environment ? LLM_URL);
  assert !(fixtureHub.environment ? HUB_FLEET_STATUS_FILE);
  assert !(fixtureHub.environment ? HUB_SECRET_INVENTORY_FILE);
  assert !(fixtureHub.environment ? HUB_FEEDS_CONFIG);
  assert !(builtins.tryEval presigned.config.systemd.services.fleetix-hub.environment.HUB_FEEDS_CONFIG).success;
  assert !(fixtureHub.serviceConfig ? BindReadOnlyPaths);
  assert fixture.config.services.harbor-db.projects."fleetix-hub".enable;
  assert lib.elem "fleetix-hub.service" fixtureMigration.requiredBy;
  assert lib.elem "fleetix-hub.service" fixtureMigration.before;
  assert lib.elem "postgresql-setup.service" fixtureMigration.requires;
  assert lib.elem "postgresql.service" fixtureHub.partOf;
  assert lib.elem "postgresql.service" fixtureActivation.wantedBy;
  assert lib.elem "harbor-db-fleetix-hub.service" fixtureActivation.requires;
  assert !(fixtureHub.unitConfig ? StartLimitBurst);
  assert fixtureHub.unitConfig.StartLimitIntervalSec == 0;
  assert fixtureHub.serviceConfig.Restart == "on-failure";
  assert fixtureHub.serviceConfig.RestartSec == 5;
  assert fixtureHub.serviceConfig.RestartPreventExitStatus == 78;
  assert fixtureHub.onFailure == [];
  assert fixtureMigration.serviceConfig.Type == "oneshot";
  assert !(fixtureMigration.serviceConfig ? Restart);
  assert fixtureCheck.serviceConfig.Type == "oneshot";
  assert remote.fleetix-hub.partOf == [];
  assert !(lib.elem "postgresql.service" remote.harbor-db-fleetix-hub-activate.wantedBy);
  assert lib.elem "network-online.target" remote.harbor-db-fleetix-hub.after;
  assert lib.elem "network-online.target" remote.harbor-db-fleetix-hub.wants;
  assert remote.harbor-db-fleetix-hub.environment.DATABASE_URL == externalUrl;
  assert !(secret.harbor-db-fleetix-hub.environment ? DATABASE_URL);
  assert remote.fleetix-hub.onFailure == ["hub-alert.service"];
  assert remote.harbor-db-fleetix-hub.onFailure == ["hub-alert.service"];
  assert lib.elem "--check" fixture.config.services.harbor-db.projects."fleetix-hub".runner.checkArgs;
  assert fixture.config.services.postgresql.enable;
  assert lib.any (db: db == "fleetix-hub") fixture.config.services.postgresql.ensureDatabases;
    pkgs.runCommand "fleetix-hub-module-eval-fixture" {
      nativeBuildInputs = [pkgs.jq];
      hubEnv = builtins.toJSON fixtureHub.environment;
      migrationLauncher = fixture.config.services.harbor-db.projects.fleetix-hub.runner.executable;
    } ''
      set -eu
      printf "%s" "$hubEnv" > env.json
      jq -e '.HUB_COMPANY_ID == "fixture-company"' env.json
      jq -e '.HUB_MIGRATE_ON_START == "0"' env.json
      jq -e 'has("HUB_SEED_FILE") | not' env.json
      jq -e 'has("LLM_URL") | not' env.json
      jq -e 'has("HUB_FLEET_STATUS_FILE") | not' env.json
      manifest=$(sed -n 's/.*--manifest //p' ${fixtureCheck.serviceConfig.ExecStart})
      test -f "$manifest"
      grep -F '"migrate","--check"' "$manifest"

      transient="Error: error communicating with database: No such file or directory (os error 2)"
      TEST_MODE=recover TEST_ERROR="$transient" TEST_STATUS=1 "$migrationLauncher" migrate --check
      test "$(wc -l < attempts)" -eq 2

      expect_failure() {
        : > attempts
        local status=0
        TEST_MODE=fail TEST_ERROR="$1" TEST_STATUS="$2" "$migrationLauncher" migrate --check || status=$?
        test "$status" -eq "$2"
        test "$(wc -l < attempts)" -eq "$3"
      }
      expect_failure "$transient" 1 3
      expect_failure "$transient" 78 1
      expect_failure "Error: schema mismatch" 1 1
      touch $out
    '';

  # Hub next to real infernix ComfyUI and llama-swap: the generated seed
  # carries the loopback ComfyUI engine plus the admission policy, and the
  # assistant points at llama-swap.
  module-eval-live = assert liveHub.environment.HUB_COMFY == "live";
  assert liveHub.environment.HUB_ASSISTANT == "live";
  assert liveHub.environment.LLM_URL == "http://127.0.0.1:8013/v1";
  assert liveHub.environment.LLM_MODEL == "qwen3.8-27b";
  assert liveHub.environment.HUB_MIGRATE_ON_START == "0";
  assert liveHub.environment.HUB_FLEET_STATUS_FILE == "/var/lib/fleet-status/fleet.json";
  assert liveHub.environment.HUB_SECRET_INVENTORY_FILE == "/var/lib/secret-inventory/inventory.json";
  # The directory, not the file: atomic replacements must stay visible.
  assert lib.elem "/var/lib/fleet-status" liveHub.serviceConfig.BindReadOnlyPaths;
  assert lib.elem "/var/lib/secret-inventory" liveHub.serviceConfig.BindReadOnlyPaths;
  # Feed keys reach the unit as credentials; the config names only their ids.
  assert lib.elem "feed-syndb-access-key-id:/run/agenix/syndb-status-key-id" liveHub.serviceConfig.LoadCredential;
  assert lib.elem "feed-syndb-secret-access-key:/run/agenix/syndb-status-secret" liveHub.serviceConfig.LoadCredential;
  assert lib.elem "comfyui.service" liveHub.after;
  assert lib.elem "llama-swap.service" liveHub.after;
  assert lib.elem "comfyui.service" liveHub.wants;
  assert lib.elem "llama-swap.service" liveHub.wants;
  assert !(lib.elem "comfyui.service" fixtureComfyHub.wants);
  assert lib.elem "llama-swap.service" fixtureComfyHub.wants;
  assert live.config.services.harbor-db.projects."fleetix-hub".enable;
  assert lib.elem "fleetix-hub.service" liveMigration.requiredBy;
  assert lib.elem "--check" live.config.services.harbor-db.projects."fleetix-hub".runner.checkArgs;
    pkgs.runCommand "fleetix-hub-module-eval-live" {
      nativeBuildInputs = [pkgs.jq];
      hubEnv = builtins.toJSON liveHub.environment;
    } ''
      set -eu
      printf "%s" "$hubEnv" > env.json
      jq -e '.HUB_COMFY == "live"' env.json
      jq -e '.HUB_ASSISTANT == "live"' env.json
      jq -e '.LLM_URL == "http://127.0.0.1:8013/v1"' env.json
      jq -e '.HUB_MIGRATE_ON_START == "0"' env.json
      jq -e '.HUB_FLEET_STATUS_FILE == "/var/lib/fleet-status/fleet.json"' env.json

      jq -e '.engines | length == 1' ${liveSeed}
      jq -e '.engines[0].id == "comfy-evalhost"' ${liveSeed}
      jq -e '.engines[0].base_url == "http://127.0.0.1:8188"' ${liveSeed}
      jq -e '.engines[0].capabilities.admission == {
        "max_in_flight": 1,
        "defer_while_llm_loaded": true,
        "llm_running_url": "http://127.0.0.1:8013/running",
        "defer_on_unknown_work": true
      }' ${liveSeed}
      jq -e '.personas == [] and .accounts == []' ${liveSeed}
      jq -e '.renewals | length == 1' ${liveSeed}
      jq -e '.renewals[0].key == "eval-token"' ${liveSeed}
      jq -e '.renewals[0].urls == ["https://example.org/oauth/apps"]' ${liveSeed}
      jq -e '.renewals[0].expiry_kind == "provider"' ${liveSeed}
      jq -e '.syndb == {
        "kind": "s3", "endpoint": "https://s3-west.nrp-nautilus.io", "region": "us-east-1",
        "bucket": "syndb-status", "key": "report.json",
        "access_key_id_file": "feed-syndb-access-key-id",
        "secret_access_key_file": "feed-syndb-secret-access-key",
        "interval_secs": 300, "stale_after_secs": 2700, "min_status": "warn"
      }' ${liveHub.environment.HUB_FEEDS_CONFIG}
      ! grep -q agenix ${liveHub.environment.HUB_FEEDS_CONFIG}
      manifest=$(sed -n 's/.*--manifest //p' ${liveCheck.serviceConfig.ExecStart})
      test -f "$manifest"
      grep -F '"migrate","--check"' "$manifest"
      touch $out
    '';
}
