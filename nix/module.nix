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
  cfg = config.services.fleetix-hub;
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

  runtimeUser = "fleetix-hub";
  runtimeGroup = "fleetix-hub";

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
    then (pkgs.formats.json {}).generate "fleetix-hub-seed.json" seedContent
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
  mkLauncher = name: exe:
    if cfg.database.urlFile != null
    then
      pkgs.writeShellScript "${name}-db-url" ''
        export DATABASE_URL="$(cat ${toString cfg.database.urlFile})"
        exec ${exe} "$@"
      ''
    else exe;
  coreExecutable = "${cfg.package}/bin/fleetix-hubd";
  # `serve` runs the product binary (it carries the packs); tokens and
  # migrations are core and always run the upstream binary.
  launcher = mkLauncher "fleetix-hub-serve" cfg.executable;
  coreLauncher = mkLauncher "fleetix-hubd" coreExecutable;

  # --- packs --------------------------------------------------------------
  enabledPacks = lib.filterAttrs (_: pack: pack.enable) cfg.packs;

  # Only enabled packs are serialized, so a disabled pack cannot make startup
  # fail with "unknown pack" and its credentials are never loaded.
  packsFile =
    if enabledPacks == {}
    then null
    else pkgs.writeText "fleetix-hub-packs.json" (builtins.toJSON (lib.mapAttrs (_: pack: pack.config) enabledPacks));
  packsArg = lib.optionalString (packsFile != null) " --packs-config ${packsFile}";

  # --- credentials --------------------------------------------------------
  credentials = lib.filterAttrs (_: value: value != null) {
    paperclip-token = cfg.credentials.paperclipToken;
    windmill-token = cfg.credentials.windmillToken;
    postiz-token = cfg.credentials.postizToken;
  };
  tokenCredentials = lib.mapAttrs' (name: token: lib.nameValuePair "token-${name}" token.file) cfg.tokens;
  # Credential ids must be `<id>-<name>`: the pack reads them from
  # `$CREDENTIALS_DIRECTORY` under that name.
  packCredentials = lib.concatLists (lib.mapAttrsToList
    (id: pack: lib.mapAttrsToList (name: path: "${id}-${name}:${path}") pack.credentials)
    enabledPacks);
  loadCredentials =
    lib.mapAttrsToList (name: path: "${name}:${path}") (credentials // tokenCredentials)
    ++ packCredentials;
  # `LoadCredential` silently keeps one entry per repeated id, and an id is
  # ambiguous: `foo` + `bar-baz` and `foo-bar` + `baz` both render
  # `foo-bar-baz`, and a pack credential can shadow a built-in `token-<name>`.
  # Assert over the complete generated list, built-ins and tokens included.
  credentialIds =
    lib.attrNames credentials
    ++ lib.attrNames tokenCredentials
    ++ lib.concatLists (lib.mapAttrsToList
      (id: pack: lib.mapAttrsToList (name: _: "${id}-${name}") pack.credentials)
      enabledPacks);
  credentialIdsUnique = lib.length credentialIds == lib.length (lib.unique credentialIds);

  tokenCommands =
    lib.mapAttrsToList (name: token: "${coreLauncher} token --name ${lib.escapeShellArg name} --role ${token.role} --token-file %d/token-${name}")
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

  # Only the publisher has a real disabled mode (the hub refuses the others).
  mode = types.enum ["fixture" "live"];
  publisherMode = types.enum ["fixture" "live" "disabled"];
in {
  imports = [harbor-db.nixosModules.default];

  options.services.fleetix-hub = {
    enable = mkEnableOption "fleetix-hub content operations hub";

    package = mkOption {
      type = types.package;
      default = self.packages.${system}.fleetix-hubd;
      defaultText = lib.literalExpression "fleetix-hub.packages.\${system}.fleetix-hubd";
      description = "Server package; must ship registry/ under share/fleetix-hub.";
    };

    executable = mkOption {
      type = types.path;
      default = "${cfg.package}/bin/fleetix-hubd";
      defaultText = lib.literalExpression ''"''${cfg.package}/bin/fleetix-hubd"'';
      description = ''
        Binary used for `serve`. A downstream product points this at its own
        entry point, for example the `canix-hub` binary, which must accept
        `serve [--packs-config FILE]`. Token registration and migrations are
        core and always run `''${cfg.package}/bin/fleetix-hubd`.
      '';
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
      default = "/var/lib/fleetix-hub/assets";
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

    fleetStatusFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        A `fleetix::status::FleetStatus` snapshot written by another tool
        (`canix fleet status --write`). The server re-reads it on every
        `/v1/fleet/status` request and never collects fleet data itself.

        The parent directory is bind-mounted read-only: the file is replaced
        atomically by the writer's timer, so the unit must see the directory
        (and therefore new inodes) rather than one bound file. The unit runs
        with `ProtectHome`, so the directory must live outside `/home`,
        `/root` and `/run/user`.
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
        default = "fleetix-hub";
        description = "PostgreSQL database name.";
      };
      url = mkOption {
        type = types.str;
        default = "postgres:///fleetix-hub?host=/run/postgresql";
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
        type = publisherMode;
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

    packs = mkOption {
      type = types.attrsOf (types.submodule ({name, ...}: {
        options = {
          enable = mkEnableOption "pack ${name}";
          config = mkOption {
            type = types.attrsOf types.anything;
            default = {};
            description = ''
              Pack config, serialized as JSON into `packs.json`. Rust
              registration does not generate Nix options, so the module checks
              the shape (an attrset that `builtins.toJSON` can render) and the
              pack parses its own fields.
            '';
          };
          credentials = mkOption {
            type = types.attrsOf types.path;
            default = {};
            description = ''
              Pack secrets as name -> file path, typically agenix paths. Each
              becomes the systemd credential `<id>-<name>`, readable by the
              pack from `$CREDENTIALS_DIRECTORY`; only the path enters the
              store.
            '';
          };
        };
      }));
      default = {};
      example = lib.literalExpression ''
        {
          plinth = {
            enable = true;
            config = {baseUrl = "https://example.com";};
            credentials.token = config.age.secrets.plinth-token.path;
          };
        }
      '';
      description = ''
        Compile-time packs installed by the executable. Enabled packs are
        rendered to `packs.json` (id -> config) and passed as
        `--packs-config`; startup fails when an id has no matching pack in the
        binary.
      '';
    };
  };

  config = mkIf cfg.enable (lib.mkMerge [
    {
      assertions = [
        {
          assertion = cfg.fleetStatusFile == null || !lib.any (home: lib.hasPrefix home (toString cfg.fleetStatusFile)) ["/home/" "/root/" "/run/user/"];
          message = "fleetix-hub: fleetStatusFile must live outside /home, /root and /run/user (the unit runs with ProtectHome).";
        }
        {
          assertion = cfg.integrations.windmill != "fixture" || cfg.integrations.comfy == "fixture";
          message = "fleetix-hub: a fixture Windmill runs procedures in-process and needs a fixture ComfyUI.";
        }
        {
          assertion = cfg.integrations.paperclip != "live" || cfg.credentials.paperclipToken != null;
          message = "fleetix-hub: live Paperclip needs credentials.paperclipToken.";
        }
        {
          assertion = cfg.integrations.windmill != "live" || cfg.credentials.windmillToken != null;
          message = "fleetix-hub: live Windmill needs credentials.windmillToken.";
        }
        {
          assertion = cfg.integrations.publisher != "live" || cfg.integrations.paperclip == "live";
          message = "fleetix-hub: a live publisher needs live Paperclip; fixture approvals cannot authorize real posts.";
        }
        {
          assertion = cfg.integrations.publisher != "live" || cfg.credentials.postizToken != null;
          message = "fleetix-hub: a live publisher needs credentials.postizToken.";
        }
        {
          assertion = allFixture || cfg.companyId != null;
          message = "fleetix-hub: companyId (the Paperclip company id) is required unless every integration is fixture.";
        }
        {
          assertion = cfg.integrations.assistant != "live" || llmUrl != null;
          message = "fleetix-hub: a live assistant needs an endpoint; set assistant.url for endpoint = custom.";
        }
        {
          assertion = cfg.database.urlFile == null || !cfg.database.createLocally;
          message = "fleetix-hub: database.urlFile is for an external database; leave database.createLocally = false.";
        }
        {
          assertion =
            lib.all (id: builtins.match "[a-z0-9-]+" id != null) (lib.attrNames enabledPacks)
            && lib.all (pack: lib.all (name: builtins.match "[A-Za-z0-9_.-]+" name != null) (lib.attrNames pack.credentials)) (lib.attrValues enabledPacks);
          message = "fleetix-hub: pack ids must match [a-z0-9-]+ and credential names [A-Za-z0-9_.-]+, so `<id>-<name>` is a valid systemd credential id.";
        }
        {
          assertion = credentialIdsUnique;
          message = "fleetix-hub: generated LoadCredential ids must be unique across credentials, tokens and packs. `foo` + `bar-baz` and `foo-bar` + `baz` both render `foo-bar-baz`, and a pack `token` + `x` collides with the token credential `token-x`; systemd silently keeps one entry per repeated id. Rename the colliding pack or credential.";
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

      systemd.services.fleetix-hub = {
        description = "fleetix-hub API and integration worker";
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
            HUB_REGISTRY_DIR = "${cfg.package}/share/fleetix-hub/registry";
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
          // optionalAttrs (cfg.fleetStatusFile != null) {HUB_FLEET_STATUS_FILE = toString cfg.fleetStatusFile;}
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
            ExecStart = "${launcher} serve${packsArg}";
            ExecStartPre = tokenCommands;
            User = runtimeUser;
            Group = runtimeGroup;
            DynamicUser = false;
            StateDirectory = "fleetix-hub";
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
          }
          // optionalAttrs (cfg.fleetStatusFile != null) {
            # Bind the directory, not the file: atomic replacement gives the
            # snapshot a new inode. Read-only is all the hub needs.
            BindReadOnlyPaths = [(builtins.dirOf (toString cfg.fleetStatusFile))];
          };
      };
    }

    # Local PostgreSQL: the migration role owns the schema and the runtime
    # role receives grants, mirroring the pink-raven deployment shape.
    (mkIf cfg.database.createLocally {
      services.postgresql = {
        enable = true;
        ensureDatabases = [cfg.database.name];
        # ensureDBOwnership needs the role and database names to match, which a
        # separate migration role or a custom database name breaks; the owner
        # is set explicitly below instead.
        ensureUsers =
          [{name = databaseOwner;}]
          ++ optional (databaseOwner != runtimeUser) {name = runtimeUser;};
      };

      systemd.services.postgresql-setup.script = lib.mkAfter (lib.concatMapStrings
        (sql: "psql -d ${lib.escapeShellArg cfg.database.name} -tAc ${lib.escapeShellArg sql}\n")
        ([
            "ALTER DATABASE ${sqlIdentifier cfg.database.name} OWNER TO ${sqlIdentifier databaseOwner};"
          ]
          ++ lib.optionals usesSeparateMigrationUser [
            "GRANT USAGE ON SCHEMA public TO ${sqlIdentifier runtimeUser};"
            "GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO ${sqlIdentifier runtimeUser};"
            "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO ${sqlIdentifier runtimeUser};"
            "ALTER DEFAULT PRIVILEGES FOR ROLE ${sqlIdentifier migrationUser} IN SCHEMA public GRANT ALL PRIVILEGES ON TABLES TO ${sqlIdentifier runtimeUser};"
            "ALTER DEFAULT PRIVILEGES FOR ROLE ${sqlIdentifier migrationUser} IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO ${sqlIdentifier runtimeUser};"
          ]));
    })

    # Schema ownership belongs to harbor-db; the runtime unit starts only
    # after the migration and its read-only check.
    {
      services.harbor-db.projects.fleetix-hub = mkIf cfg.migrations.enable {
        enable = true;
        description = "fleetix-hub database migrations";
        runner =
          if cfg.database.urlFile != null
          then {
            command = "${coreLauncher} migrate";
            checkCommand = "${coreLauncher} migrate --check";
          }
          else {
            executable = coreExecutable;
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
        runtimeUnits = ["fleetix-hub.service"];
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
