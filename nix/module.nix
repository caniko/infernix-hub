# NixOS module for the hub API + embedded worker.
#
# Secrets arrive as systemd credentials (point them at agenix paths); the
# service reads `$CREDENTIALS_DIRECTORY/<name>` via the `*_TOKEN_FILE` env.
# Every integration defaults to `fixture`; switching one to `live` is an
# explicit, per-integration decision.
#
# Database schema is owned by harbor-db: migrations run as a separate role
# before the runtime unit, and the server refuses to start on a stale schema
# (HUB_MIGRATE_ON_START=0).
{
  self,
  harbor-rs,
  harbor-db,
}: {
  config,
  lib,
  pkgs,
  options,
  ...
}: let
  cfg = config.services.infernix-hub;
  inherit
    (lib)
    mkEnableOption
    mkIf
    mkOption
    optional
    optionalAttrs
    optionals
    types
    ;

  runtimeUser = "infernix-hub";
  runtimeGroup = "infernix-hub";

  system = pkgs.stdenv.hostPlatform.system;

  # Integration defaults are read from the infernix stack only when a host
  # imports it; the hub module works standalone.
  hasInfernix = options.services ? infernix;
  infernixCfg =
    if hasInfernix
    then config.services.infernix
    else {};
  infernixOpt = name: infernixCfg.${name} or {};

  comfyCfg = infernixOpt "comfyui";
  llamaCfg = infernixOpt "llama-swap";
  lbCfg = infernixOpt "loadBalancer";
  fleetCfg = infernixOpt "fleet";

  comfyEnabled = comfyCfg.enable or false;
  llamaEnabled = llamaCfg.enable or false;
  lbEnabled = (lbCfg.enable or false) || ((fleetCfg.loadBalancer or {}).enable or false);

  # A wildcard listener is reached over loopback from this host.
  urlHost = host:
    if host == "0.0.0.0" || host == "::" || host == "[::]"
    then "127.0.0.1"
    else host;

  comfyUrl = "http://${urlHost (comfyCfg.host or "127.0.0.1")}:${toString (comfyCfg.port or 8188)}";
  llamaUrl = "http://${urlHost (llamaCfg.host or "0.0.0.0")}:${toString (llamaCfg.port or 8013)}";
  lbUrl = "http://${urlHost (lbCfg.host or "192.168.178.31")}:${toString (lbCfg.port or 8014)}";

  windmillEnabled =
    (config.services ? windmill && (config.services.windmill.enable or false))
    || config.systemd.services ? windmill-server;
  windmillPort =
    if config.services ? windmill
    then config.services.windmill.serverPort or 8000
    else 8000;
  n8nEnabled = config.services ? n8n && (config.services.n8n.enable or false);

  # --- assistant endpoint -------------------------------------------------
  llmUrl =
    if cfg.assistant.endpoint == "custom"
    then cfg.assistant.url
    else if cfg.assistant.endpoint == "load-balancer"
    then "${lbUrl}/v1"
    else "${llamaUrl}/v1";
  assistantAvailable =
    llmUrl
    != null
    && (
      if cfg.assistant.endpoint == "custom"
      then true
      else if cfg.assistant.endpoint == "load-balancer"
      then lbEnabled
      else llamaEnabled
    );

  # --- seed ---------------------------------------------------------------
  admission = {
    max_in_flight = cfg.gpuAdmission.maxInFlight;
    defer_while_llm_loaded = cfg.gpuAdmission.deferWhileLlmLoaded;
    llm_running_url = cfg.gpuAdmission.llmRunningUrl;
    defer_on_unknown_work = cfg.gpuAdmission.deferOnUnknownEngineWork;
  };

  allEngines =
    optionalAttrs comfyEnabled {
      "comfy-${config.networking.hostName}" = {
        baseUrl = comfyUrl;
        capabilities = {};
      };
    }
    // cfg.engines;

  engineRecords =
    lib.mapAttrsToList (id: engine: {
      inherit id;
      base_url = engine.baseUrl;
      capabilities = engine.capabilities // {inherit admission;};
    })
    allEngines;

  seedContent = {
    engines = engineRecords;
    personas = cfg.personas;
    accounts = cfg.accounts;
  };

  seedFile =
    if cfg.seedFile != null
    then cfg.seedFile
    else if engineRecords != [] || cfg.personas != [] || cfg.accounts != []
    then (pkgs.formats.json {}).generate "infernix-hub-seed.json" seedContent
    else null;

  # --- database -----------------------------------------------------------
  usesSeparateMigrationUser =
    cfg.migrations.enable
    && cfg.database.migrationsUser != null
    && cfg.database.migrationsUser != runtimeUser;
  migrationUser =
    if cfg.migrations.enable && cfg.database.migrationsUser != null
    then cfg.database.migrationsUser
    else runtimeUser;
  migrationGroup =
    if cfg.database.migrationsGroup != null
    then cfg.database.migrationsGroup
    else runtimeGroup;
  # The local database owner is whoever applies the schema.
  databaseOwner = migrationUser;

  sqlIdentifier = value: "\"" + builtins.replaceStrings ["\""] ["\"\""] value + "\"";

  # A raw URL file (agenix/secret mount) is read at exec time; nothing but the
  # path enters the store.
  launcher =
    if cfg.database.urlFile != null
    then
      pkgs.writeShellScript "infernix-hubd-db-url" ''
        export DATABASE_URL="$(cat ${toString cfg.database.urlFile})"
        exec ${cfg.package}/bin/infernix-hubd "$@"
      ''
    else "${cfg.package}/bin/infernix-hubd";

  # --- credentials --------------------------------------------------------
  credentials = lib.filterAttrs (_: value: value != null) {
    paperclip-token = cfg.credentials.paperclipToken;
    windmill-token = cfg.credentials.windmillToken;
    postiz-token = cfg.credentials.postizToken;
  };
  tokenCredentials = lib.mapAttrs' (name: token: lib.nameValuePair "token-${name}" token.file) cfg.tokens;
  loadCredentials = lib.mapAttrsToList (name: path: "${name}:${path}") (credentials // tokenCredentials);

  tokenCommands =
    lib.mapAttrsToList (name: token: "${launcher} token --name ${lib.escapeShellArg name} --role ${token.role} --token-file %d/token-${name}")
    cfg.tokens;

  # --- unit ordering ------------------------------------------------------
  integrationUnits =
    optional comfyEnabled "comfyui.service"
    ++ optional (cfg.assistant.endpoint == "llama-swap" && llamaEnabled) "llama-swap.service"
    ++ optional (cfg.assistant.endpoint == "load-balancer" && lbEnabled) "infernix-lb.service";
  passiveUnits =
    optional windmillEnabled "windmill-server.service"
    ++ optional n8nEnabled "n8n.service";

  allIntegrations = [
    cfg.integrations.paperclip
    cfg.integrations.windmill
    cfg.integrations.comfy
    cfg.integrations.publisher
    cfg.integrations.assistant
  ];
  allFixture = lib.all (mode: mode != "live") allIntegrations;

  mode = types.enum ["fixture" "live" "disabled"];
in {
  imports = [harbor-db.nixosModules.default];

  options.services.infernix-hub = {
    enable = mkEnableOption "infernix-hub content operations hub";

    package = mkOption {
      type = types.package;
      default = self.packages.${system}.infernix-hubd;
      defaultText = lib.literalExpression "infernix-hub.packages.\${system}.infernix-hubd";
      description = "Server package; must ship registry/ under share/infernix-hub.";
    };

    bind = mkOption {
      type = types.str;
      default = "127.0.0.1:8470";
      description = "Address and port the HTTP API binds.";
    };

    publicUrl = mkOption {
      type = types.str;
      default = "http://${cfg.bind}";
      description = "URL Windmill procedures use to post receipts and dispatch timers.";
    };

    companyId = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Paperclip company id. Required unless every integration is fixture.";
    };

    assetDir = mkOption {
      type = types.path;
      default = "/var/lib/infernix-hub/assets";
      description = "Durable content-addressed asset volume.";
    };

    seedFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        Explicit engines/personas/accounts seed. When null the module renders
        one from `engines`, `personas`, `accounts` and the ComfyUI integration.
      '';
    };

    database = {
      createLocally = mkOption {
        type = types.bool;
        default = true;
        description = "Provision a local PostgreSQL database, roles, and grants.";
      };
      name = mkOption {
        type = types.str;
        default = "infernix-hub";
        description = "PostgreSQL database name.";
      };
      url = mkOption {
        type = types.str;
        default = "postgres:///infernix-hub?host=/run/postgresql";
        description = ''
          Connection URL without an embedded role: peer auth over the unix
          socket resolves the role from the system user (runtime user for the
          service, migrationsUser for migrations).
        '';
      };
      urlFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = ''
          File containing an external DATABASE_URL, typically an agenix
          secret. Read at exec time and exported by a private launcher; the
          contents never enter the store. Implies createLocally = false.
        '';
      };
      migrationsUser = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          System user that runs migrations. Defaults to the runtime user.
          Set this to a privileged operator account when the runtime role
          must not own the schema.
        '';
      };
      migrationsGroup = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "System group for the migration unit; defaults to the runtime group.";
      };
    };

    migrations.enable = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Run migrations through harbor-db before the runtime unit. When
        disabled the server applies migrations itself at startup.
      '';
    };

    integrations = {
      paperclip = mkOption {
        type = mode;
        default = "fixture";
      };
      windmill = mkOption {
        type = mode;
        default = "fixture";
      };
      comfy = mkOption {
        type = mode;
        default =
          if comfyEnabled
          then "live"
          else "fixture";
        defaultText = lib.literalExpression ''"fixture"'';
        description = "ComfyUI adapter; defaults to live when services.infernix.comfyui is enabled.";
      };
      publisher = mkOption {
        type = mode;
        default = "fixture";
      };
      assistant = mkOption {
        type = mode;
        default =
          if assistantAvailable
          then "live"
          else "fixture";
        defaultText = lib.literalExpression ''"fixture"'';
        description = "Assistant adapter; defaults to live when an LLM endpoint is available.";
      };
    };

    assistant = {
      endpoint = mkOption {
        type = types.enum ["llama-swap" "load-balancer" "custom"];
        default =
          if lbEnabled
          then "load-balancer"
          else "llama-swap";
        description = ''
          Where the assistant sends OpenAI-compatible chat requests. The
          default prefers the host load balancer, then llama-swap.
        '';
      };
      url = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Base URL (including /v1) when endpoint = custom.";
      };
      model = mkOption {
        type = types.str;
        default = "qwen3.8-27b";
        description = "Model id sent to the assistant endpoint.";
      };
    };

    paperclip = {
      url = mkOption {
        type = types.str;
        default = "http://127.0.0.1:3117";
        description = "Paperclip board base URL (home-manager user service on atlas).";
      };
    };

    windmill = {
      url = mkOption {
        type = types.str;
        default =
          if windmillEnabled
          then "http://127.0.0.1:${toString windmillPort}"
          else "http://127.0.0.1:8040";
        description = "Windmill base URL; defaults to the host server's port when present.";
      };
      workspace = mkOption {
        type = types.str;
        default = "studio";
        description = "Windmill workspace the procedures run in.";
      };
    };

    engines = mkOption {
      type = types.attrsOf (types.submodule {
        options = {
          baseUrl = mkOption {
            type = types.str;
            description = "ComfyUI base URL.";
          };
          capabilities = mkOption {
            type = types.attrsOf types.anything;
            default = {};
            description = "Engine capability claims; admission is added automatically.";
          };
        };
      });
      default = {};
      description = ''
        Extra engines keyed by id, e.g. a remote ComfyUI reached over
        wg-home. Do not health-probe an on-demand instance.
      '';
    };

    personas = mkOption {
      type = types.listOf (types.submodule {
        options = {
          name = mkOption {type = types.str;};
          voice = mkOption {type = types.str;};
          rules = mkOption {
            type = types.listOf types.str;
            default = [];
          };
          avoid = mkOption {
            type = types.listOf types.str;
            default = [];
          };
          world = mkOption {
            type = types.listOf types.str;
            default = [];
          };
        };
      });
      default = [];
      description = "Seed personas; each becomes accepted canon revision 1.";
    };

    accounts = mkOption {
      type = types.listOf (types.submodule {
        options = {
          id = mkOption {type = types.str;};
          platform = mkOption {type = types.str;};
          handle = mkOption {type = types.str;};
          transport = mkOption {
            type = types.str;
            default = "fixture";
          };
          persona = mkOption {
            type = types.nullOr types.str;
            default = null;
          };
          capabilities = mkOption {
            type = types.submodule {
              options = {
                publish = mkOption {
                  type = types.bool;
                  default = true;
                };
                comments = mkOption {
                  type = types.bool;
                  default = true;
                };
                mentions = mkOption {
                  type = types.bool;
                  default = true;
                };
                replies = mkOption {
                  type = types.bool;
                  default = true;
                };
                dms = mkOption {
                  type = types.bool;
                  default = false;
                };
                analytics = mkOption {
                  type = types.bool;
                  default = false;
                };
              };
            };
            default = {};
          };
        };
      });
      default = [];
      description = "Seed accounts referencing a persona by name.";
    };

    gpuAdmission = {
      maxInFlight = mkOption {
        type = types.ints.positive;
        default = 1;
        description = "Concurrent engine jobs the hub may admit.";
      };
      deferWhileLlmLoaded = mkOption {
        type = types.bool;
        default = llamaEnabled;
        description = "Hold engine work while a local llama-server is resident.";
      };
      llmRunningUrl = mkOption {
        type = types.nullOr types.str;
        default =
          if llamaEnabled
          then "${llamaUrl}/running"
          else null;
        description = "llama-swap /running probe; null disables the check.";
      };
      deferOnUnknownEngineWork = mkOption {
        type = types.bool;
        default = true;
        description = "Defer when engine load cannot be determined; fail closed.";
      };
    };

    tokens = mkOption {
      type = types.attrsOf (types.submodule {
        options = {
          role = mkOption {
            type = types.enum ["operator" "agent" "procedure" "ingest"];
          };
          file = mkOption {
            type = types.path;
            description = "Agenix/secret path holding the bearer token (min 24 chars).";
          };
        };
      });
      default = {};
      description = "Bearer tokens registered idempotently before the server starts.";
    };

    environment = mkOption {
      type = types.attrsOf types.str;
      default = {};
      example = {
        HUB_PUBLIC_URL = "https://hub.example.com";
      };
      description = "Extra environment variables for the runtime unit; overrides module defaults.";
    };

    credentials = {
      paperclipToken = mkOption {
        type = types.nullOr types.path;
        default = null;
      };
      windmillToken = mkOption {
        type = types.nullOr types.path;
        default = null;
      };
      postizToken = mkOption {
        type = types.nullOr types.path;
        default = null;
      };
    };
  };

  config = mkIf cfg.enable (lib.mkMerge [
    {
      assertions = [
        {
          assertion = cfg.integrations.windmill != "fixture" || cfg.integrations.comfy == "fixture";
          message = "infernix-hub: a fixture Windmill runs procedures in-process and needs a fixture ComfyUI.";
        }
        {
          assertion = cfg.integrations.paperclip != "live" || cfg.credentials.paperclipToken != null;
          message = "infernix-hub: live Paperclip needs credentials.paperclipToken.";
        }
        {
          assertion = cfg.integrations.windmill != "live" || cfg.credentials.windmillToken != null;
          message = "infernix-hub: live Windmill needs credentials.windmillToken.";
        }
        {
          assertion = cfg.integrations.publisher != "live" || cfg.credentials.postizToken != null;
          message = "infernix-hub: a live publisher needs credentials.postizToken.";
        }
        {
          assertion = allFixture || cfg.companyId != null;
          message = "infernix-hub: companyId (the Paperclip company id) is required unless every integration is fixture.";
        }
        {
          assertion = cfg.integrations.assistant != "live" || llmUrl != null;
          message = "infernix-hub: a live assistant needs an endpoint; set assistant.url for endpoint = custom.";
        }
        {
          assertion = cfg.database.urlFile == null || !cfg.database.createLocally;
          message = "infernix-hub: database.urlFile is for an external database; leave database.createLocally = false.";
        }
      ];

      users.groups.${runtimeGroup} = {};
      users.users.${runtimeUser} = {
        isSystemUser = true;
        group = runtimeGroup;
      };

      systemd.tmpfiles.rules = [
        "d ${toString cfg.assetDir} 0750 ${runtimeUser} ${runtimeGroup} -"
      ];

      systemd.services.infernix-hub = {
        description = "infernix-hub API and integration worker";
        wantedBy = ["multi-user.target"];
        after =
          ["network-online.target" "postgresql.service"]
          ++ integrationUnits
          ++ passiveUnits;
        wants = ["network-online.target"] ++ integrationUnits;
        requires = optional cfg.database.createLocally "postgresql.service";
        environment =
          {
            HUB_BIND = cfg.bind;
            HUB_PUBLIC_URL = cfg.publicUrl;
            HUB_ASSET_DIR = toString cfg.assetDir;
            HUB_REGISTRY_DIR = "${cfg.package}/share/infernix-hub/registry";
            HUB_PAPERCLIP = cfg.integrations.paperclip;
            HUB_WINDMILL = cfg.integrations.windmill;
            HUB_COMFY = cfg.integrations.comfy;
            HUB_PUBLISHER = cfg.integrations.publisher;
            HUB_ASSISTANT = cfg.integrations.assistant;
            HUB_COMPANY_ID =
              if cfg.companyId != null
              then cfg.companyId
              else "fixture-company";
            PAPERCLIP_URL = cfg.paperclip.url;
            WINDMILL_URL = cfg.windmill.url;
            WINDMILL_WORKSPACE = cfg.windmill.workspace;
            RUST_LOG = "info";
          }
          // optionalAttrs (seedFile != null) {HUB_SEED_FILE = toString seedFile;}
          // optionalAttrs (cfg.database.urlFile == null) {DATABASE_URL = cfg.database.url;}
          // optionalAttrs cfg.migrations.enable {HUB_MIGRATE_ON_START = "0";}
          // optionalAttrs (cfg.integrations.assistant == "live" && llmUrl != null) {
            LLM_URL = llmUrl;
            LLM_MODEL = cfg.assistant.model;
          }
          // optionalAttrs (credentials ? paperclip-token) {PAPERCLIP_TOKEN_FILE = "%d/paperclip-token";}
          // optionalAttrs (credentials ? windmill-token) {WINDMILL_TOKEN_FILE = "%d/windmill-token";}
          // optionalAttrs (credentials ? postiz-token) {POSTIZ_TOKEN_FILE = "%d/postiz-token";}
          // cfg.environment;
        serviceConfig =
          harbor-rs.lib.hardeningProfiles.network-service
          // {
            ExecStart = "${launcher} serve";
            ExecStartPre = tokenCommands;
            User = runtimeUser;
            Group = runtimeGroup;
            DynamicUser = false;
            StateDirectory = "infernix-hub";
            StateDirectoryMode = "0750";
            LoadCredential = loadCredentials;
            Restart = "on-failure";
            RestartSec = 5;
            UMask = "0027";
            # The Postgres socket and the hub's HTTP listener need these.
            CapabilityBoundingSet = "";
            RestrictAddressFamilies = ["AF_UNIX" "AF_INET" "AF_INET6"];
            PrivateDevices = true;
            SystemCallFilter = ["@system-service" "~@privileged"];
            ReadWritePaths = [(toString cfg.assetDir)];
          };
      };
    }

    # Local PostgreSQL: the migration role owns the schema and the runtime
    # role receives grants, mirroring the pink-raven deployment shape.
    (mkIf cfg.database.createLocally {
      services.postgresql = {
        enable = true;
        ensureDatabases = [cfg.database.name];
        ensureUsers =
          [
            {
              name = databaseOwner;
              ensureDBOwnership = true;
            }
          ]
          ++ optional (databaseOwner != runtimeUser) {name = runtimeUser;};
      };

      systemd.services.postgresql-setup = mkIf usesSeparateMigrationUser {
        script = lib.mkAfter ''
          psql -d ${lib.escapeShellArg cfg.database.name} -tAc "GRANT USAGE ON SCHEMA public TO ${sqlIdentifier runtimeUser};"
          psql -d ${lib.escapeShellArg cfg.database.name} -tAc "GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO ${sqlIdentifier runtimeUser};"
          psql -d ${lib.escapeShellArg cfg.database.name} -tAc "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO ${sqlIdentifier runtimeUser};"
          psql -d ${lib.escapeShellArg cfg.database.name} -tAc "ALTER DEFAULT PRIVILEGES FOR ROLE ${sqlIdentifier migrationUser} IN SCHEMA public GRANT ALL PRIVILEGES ON TABLES TO ${sqlIdentifier runtimeUser};"
          psql -d ${lib.escapeShellArg cfg.database.name} -tAc "ALTER DEFAULT PRIVILEGES FOR ROLE ${sqlIdentifier migrationUser} IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO ${sqlIdentifier runtimeUser};"
        '';
      };
    })

    # Schema ownership belongs to harbor-db; the runtime unit starts only
    # after the migration and its read-only check.
    {
      services.harbor-db.projects.infernix-hub = mkIf cfg.migrations.enable {
        enable = true;
        description = "infernix-hub database migrations";
        runner =
          if cfg.database.urlFile != null
          then {
            command = "${launcher} migrate";
            checkCommand = "${launcher} migrate --check";
          }
          else {
            inherit (cfg) package;
            executable = "bin/infernix-hubd";
            args = ["migrate"];
            checkArgs = ["migrate" "--check"];
          };
        user = migrationUser;
        group = migrationGroup;
        environment =
          {
            RUST_LOG = "info";
            # An omitted URL role must resolve to this OS identity for peer auth.
            PGUSER = migrationUser;
          }
          // optionalAttrs (cfg.database.urlFile == null) {DATABASE_URL = cfg.database.url;};
        runtimeUnits = ["infernix-hub.service"];
        postgres =
          {
            enable = cfg.database.createLocally;
            setupUnits = optional cfg.database.createLocally "postgresql-setup.service";
            grants = {
              enable = cfg.database.createLocally && usesSeparateMigrationUser;
              runtimeRole = runtimeUser;
            };
          }
          // optionalAttrs (cfg.database.urlFile == null) {databaseUrl = cfg.database.url;};
      };
    }
  ]);
}
