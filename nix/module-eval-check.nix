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
      modules = modules;
    };

  fixture = mkSystem [
    module
    ({config, ...}: {
      system.stateVersion = config.system.nixos.release;
      networking.hostName = "evalhost";
      services.infernix-hub.enable = true;
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
        pkgs = pkgs;
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
      services.infernix-hub.enable = true;
    })
  ];

  fixtureHub = fixture.config.systemd.services.infernix-hub;
  fixtureMigration = fixture.config.systemd.services."harbor-db-infernix-hub";
  fixtureCheck = fixture.config.systemd.services."harbor-db-infernix-hub-check";

  liveHub = live.config.systemd.services.infernix-hub;
  liveMigration = live.config.systemd.services."harbor-db-infernix-hub";
  liveCheck = live.config.systemd.services."harbor-db-infernix-hub-check";
  liveSeed = liveHub.environment.HUB_SEED_FILE;
in {
  # Hub alone, every integration fixture: fixture company, no seed, and the
  # harbor-db migration gate enabled with local PostgreSQL.
  module-eval-fixture = assert fixtureHub.environment.HUB_COMPANY_ID == "fixture-company";
  assert fixtureHub.environment.HUB_MIGRATE_ON_START == "0";
  assert !(fixtureHub.environment ? HUB_SEED_FILE);
  assert !(fixtureHub.environment ? LLM_URL);
  assert fixture.config.services.harbor-db.projects."infernix-hub".enable;
  assert lib.elem "infernix-hub.service" fixtureMigration.requiredBy;
  assert lib.elem "infernix-hub.service" fixtureMigration.before;
  assert lib.elem "--check" fixture.config.services.harbor-db.projects."infernix-hub".runner.checkArgs;
  assert fixture.config.services.postgresql.enable;
  assert lib.any (db: db == "infernix-hub") fixture.config.services.postgresql.ensureDatabases;
    pkgs.runCommand "infernix-hub-module-eval-fixture" {
      nativeBuildInputs = [pkgs.jq];
      hubEnv = builtins.toJSON fixtureHub.environment;
    } ''
      set -eu
      printf "%s" "$hubEnv" > env.json
      jq -e '.HUB_COMPANY_ID == "fixture-company"' env.json
      jq -e '.HUB_MIGRATE_ON_START == "0"' env.json
      jq -e 'has("HUB_SEED_FILE") | not' env.json
      jq -e 'has("LLM_URL") | not' env.json
      manifest=$(sed -n 's/.*--manifest //p' ${fixtureCheck.serviceConfig.ExecStart})
      test -f "$manifest"
      grep -F '"migrate","--check"' "$manifest"
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
  assert lib.elem "comfyui.service" liveHub.after;
  assert lib.elem "llama-swap.service" liveHub.after;
  assert lib.elem "comfyui.service" liveHub.wants;
  assert lib.elem "llama-swap.service" liveHub.wants;
  assert live.config.services.harbor-db.projects."infernix-hub".enable;
  assert lib.elem "infernix-hub.service" liveMigration.requiredBy;
  assert lib.elem "--check" live.config.services.harbor-db.projects."infernix-hub".runner.checkArgs;
    pkgs.runCommand "infernix-hub-module-eval-live" {
      nativeBuildInputs = [pkgs.jq];
      hubEnv = builtins.toJSON liveHub.environment;
    } ''
      set -eu
      printf "%s" "$hubEnv" > env.json
      jq -e '.HUB_COMFY == "live"' env.json
      jq -e '.HUB_ASSISTANT == "live"' env.json
      jq -e '.LLM_URL == "http://127.0.0.1:8013/v1"' env.json
      jq -e '.HUB_MIGRATE_ON_START == "0"' env.json

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
      manifest=$(sed -n 's/.*--manifest //p' ${liveCheck.serviceConfig.ExecStart})
      test -f "$manifest"
      grep -F '"migrate","--check"' "$manifest"
      touch $out
    '';
}
