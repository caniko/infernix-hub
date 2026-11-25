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

  renewalRecords =
    map (r: {
      inherit (r) key title procedure account urls;
      cadence_days = r.cadenceDays;
      warn_days = r.warnDays;
      due_at = r.dueAt;
      expiry_kind = r.expiryKind;
      inherit (r) owner;
    })
    cfg.renewals;

  seedContent = {
    engines = engineRecords;
    inherit (cfg) personas;
    inherit (cfg) accounts;
    renewals = renewalRecords;
  };

  seedFile =
    if cfg.seedFile != null
    then cfg.seedFile
    else if engineRecords != [] || cfg.personas != [] || cfg.accounts != [] || cfg.renewals != []
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
        DATABASE_URL="$(${pkgs.coreutils}/bin/cat -- ${lib.escapeShellArg (toString cfg.database.urlFile)})" || exit 78
        if [[ -z "$DATABASE_URL" ]]; then
          echo "fleetix-hub: database.urlFile is empty" >&2
          exit 78
        fi
        export DATABASE_URL
        exec ${exe} "$@"
      ''
    else exe;
  coreExecutable = "${cfg.package}/bin/fleetix-hubd";
  # `serve` runs the product binary (it carries the packs); tokens and
  # migrations are core and always run the upstream binary.
  launcher = mkLauncher "fleetix-hub-serve" cfg.executable;
  coreLauncher = mkLauncher "fleetix-hubd" coreExecutable;
  migrationLauncher = pkgs.writeShellScript "fleetix-hub-migrate" ''
    for attempt in 1 2 3; do
      # Keep stdout separate so only the daemon's top-level SQLx error can
      # trigger a retry. SQLx already waits on TCP refusal until pool timeout.
      status=0
      { error="$(${coreLauncher} "$@" 2>&1 >&3)"; } 3>&1 || status=$?
      [[ -z "$error" ]] || printf '%s\n' "$error" >&2
      if (( status != 1 || attempt == 3 )); then
        exit "$status"
      fi
      case "''${error%%$'\n'*}" in
        "Error: pool timed out while waiting for an open connection" | \
        "Error: error communicating with database: Connection refused (os error 111)" | \
        "Error: error communicating with database: No such file or directory (os error 2)")
          echo "fleetix-hub: PostgreSQL not ready; retrying migration command" >&2
          ${pkgs.coreutils}/bin/sleep 2
          ;;
        *) exit "$status" ;;
      esac
    done
  '';

  # --- packs --------------------------------------------------------------
  enabledPacks = lib.filterAttrs (_: pack: pack.enable) cfg.packs;

  # Only enabled packs are serialized, so a disabled pack cannot make startup
  # fail with "unknown pack" and its credentials are never loaded.
  packsFile =
    if enabledPacks == {}
    then null
    else pkgs.writeText "fleetix-hub-packs.json" (builtins.toJSON (lib.mapAttrs (_: pack: pack.config) enabledPacks));
  packsArg = lib.optionalString (packsFile != null) " --packs-config ${packsFile}";

  # --- status feeds -------------------------------------------------------
  # Credentials stay in their agenix files: each becomes a systemd credential
  # and the config names only its id, which the hub resolves against
  # `$CREDENTIALS_DIRECTORY`.
  feedCredentials = lib.concatLists (lib.mapAttrsToList (id: feed: [
      "feed-${id}-access-key-id:${feed.credentials.accessKeyId}"
      "feed-${id}-secret-access-key:${feed.credentials.secretAccessKey}"
    ])
    cfg.feeds);
  feedsFile =
    if cfg.feeds == {}
    then null
    else
      pkgs.writeText "fleetix-hub-feeds.json" (builtins.toJSON (lib.mapAttrs (id: feed: {
          inherit (feed) kind region bucket key;
          # Checked here, not only in an assertion: evaluating the config alone
          # must never put userinfo or a presigned query into a .drv. The
          # message names no part of the URL.
          endpoint =
            if builtins.match "https://[A-Za-z0-9.-]+(:[0-9]+)?/?" feed.endpoint != null
            then feed.endpoint
            else throw "fleetix-hub: feed ${id}: endpoint must be a bare https origin (https://host[:port]).";
          access_key_id_file = "feed-${id}-access-key-id";
          secret_access_key_file = "feed-${id}-secret-access-key";
          interval_secs = feed.intervalSecs;
          stale_after_secs = feed.staleAfterSecs;
          min_status = feed.minStatus;
        })
        cfg.feeds));

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
    ++ packCredentials
    ++ feedCredentials;
  # `LoadCredential` silently keeps one entry per repeated id, and an id is
  # ambiguous: `foo` + `bar-baz` and `foo-bar` + `baz` both render
  # `foo-bar-baz`, and a pack credential can shadow a built-in `token-<name>`.
  # Assert over the complete generated list, built-ins and tokens included.
  credentialIds =
    lib.attrNames credentials
    ++ lib.attrNames tokenCredentials
    ++ lib.concatLists (lib.mapAttrsToList
      (id: pack: lib.mapAttrsToList (name: _: "${id}-${name}") pack.credentials)
      enabledPacks)
    ++ map (entry: lib.head (lib.splitString ":" entry)) feedCredentials;
  credentialIdsUnique = lib.length credentialIds == lib.length (lib.unique credentialIds);

  tokenCommands =
    lib.mapAttrsToList (name: token: "${coreLauncher} token --name ${lib.escapeShellArg name} --role ${token.role} --token-file %d/token-${name}")
    cfg.tokens;

  # --- unit ordering ------------------------------------------------------
  integrationUnits =
    optional (comfyEnabled && cfg.integrations.comfy == "live") "comfyui.service"
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

  nullableString = description:
    mkOption {
      type = types.nullOr types.str;
      default = null;
      inherit description;
    };
  positive = default: description:
    mkOption {
      type = types.ints.positive;
      inherit default description;
    };
  jsonFile = name: value: pkgs.writeText "fleetix-hub-${name}.json" (builtins.toJSON value);
  nonSecretHttpUrl = url:
    builtins.match ''https?://[^/@?#\\[:space:][:cntrl:]]+(/[^?#\\[:space:][:cntrl:]]*)?'' url
    != null
    && !lib.hasInfix "/private-" url;
  mail = cfg.mail;
  calendar = cfg.calendar;
  mailConfig = {
    sync_interval_seconds = mail.syncIntervalSeconds;
    archive_raw_dir = mail.archiveRawDir;
    accounts = lib.mapAttrsToList (_: account:
      {
        inherit (account) owner label provider transport address enabled;
        password_file = runtimeCredential account.passwordFile;
        export_dir = account.exportDir;
      }
      // optionalAttrs (account.transport == "imap") {
        inherit (account.imap) host port username folders;
        tls_mode = account.imap.tls;
      })
    mail.accounts;
    bi = {
      inherit (mail.bi) enable;
      interval_seconds = mail.bi.llm.intervalSeconds;
      max_batch_per_tick = mail.bi.llm.maxBatchPerTick;
      skip_junk = mail.bi.llm.skipJunk;
      llm = {
        inherit (mail.bi.llm) enable model;
        base_url =
          if nonSecretHttpUrl mail.bi.llm.baseUrl
          then mail.bi.llm.baseUrl
          else throw "fleetix-hub: mail.bi.llm.baseUrl must be a credential-free HTTP(S) endpoint.";
      };
    };
  };
  calendarConfig = {
    sync_interval_seconds = calendar.syncIntervalSeconds;
    sources =
      lib.mapAttrsToList (_: source: {
        inherit (source) owner label provider visibility color enabled username;
        # Only an explicitly public URL can be rendered into a store file.
        url =
          if source.urlFile != null
          then null
          else if source.url == null || (source.provider != "google_ics" && nonSecretHttpUrl source.url)
          then source.url
          else throw "fleetix-hub: use calendar.sources.<name>.urlFile for private, Google ICS, or credential-bearing URLs.";
        url_file = runtimeCredential source.urlFile;
        password_file = runtimeCredential source.passwordFile;
        service_account_file = runtimeCredential source.serviceAccountFile;
        calendar_id = source.calendarId;
      })
      calendar.sources;
  };
  runtimeFiles = lib.filter (path: path != null) (
    lib.optionals mail.enable (map (account: account.passwordFile) (lib.attrValues mail.accounts))
    ++ lib.optionals calendar.enable (lib.concatMap (source: [source.urlFile source.passwordFile source.serviceAccountFile]) (lib.attrValues calendar.sources))
  );
  runtimeCredential = path:
    if path == null || (builtins.match "/[^[:cntrl:]]+" path != null && !builtins.hasContext path && !(lib.hasPrefix "/nix/store/" path) && path != "/nix/store")
    then path
    else throw "fleetix-hub: credential files must be absolute runtime paths outside the Nix store.";
  optionalRuntimePath = path: let
    value = "-${lib.replaceStrings ["%"] ["%%"] (runtimeCredential path)}";
  in
    if builtins.match ''[^[:space:]"\\]+'' value != null
    then value
    else builtins.toJSON value;
  archiveEnabled = mail.enable && mail.archiveRawDir != null;

  ownerOption = mkOption {
    type = types.str;
    description = "Hub owner to whom the data belongs.";
  };
  labelOption = name:
    mkOption {
      type = types.str;
      default = name;
      description = "Display label, unique per owner.";
    };
  enabledOption = mkOption {
    type = types.bool;
    default = true;
    description = "Whether this entry participates in sync.";
  };
in {
  imports = [harbor-db.nixosModules.default];

  options.services.fleetix-hub = {
    enable = mkEnableOption "fleetix-hub content operations hub";

    systemd.onFailure = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Systemd units to start when the hub or its migration gate enters the failed state.";
    };

    principalOwners = mkOption {
      type = types.attrsOf types.str;
      default = {};
      description = "Map authenticated principal names to mail and calendar owners.";
    };

    mail = {
      enable = mkEnableOption "Fleetix Hub mail ingestion";
      syncIntervalSeconds = positive 300 "Seconds between mail sync passes.";
      archiveRawDir = nullableString "Directory for optional private raw mail archival.";
      accounts = mkOption {
        type = types.attrsOf (types.submodule ({name, ...}: {
          options = {
            owner = ownerOption;
            label = labelOption name;
            enabled = enabledOption;
            provider = mkOption {
              type = types.enum ["gmail" "protonmail" "tutanota" "imap" "export_dir"];
              description = "Mail provider.";
            };
            transport = mkOption {
              type = types.enum ["imap" "export_dir" "client_push"];
              description = "How messages are ingested.";
            };
            address = mkOption {
              type = types.str;
              description = "Account email address.";
            };
            imap = {
              host = nullableString "IMAP server host, without a scheme.";
              port = mkOption {
                type = types.nullOr types.port;
                default = null;
                description = "IMAP port; null uses the daemon's transport default.";
              };
              tls = mkOption {
                type = types.enum ["tls" "starttls" "plain"];
                default = "tls";
                description = "IMAP security mode. Plain transport is restricted to loopback.";
              };
              username = nullableString "IMAP login name.";
              folders = mkOption {
                type = types.listOf types.str;
                default = ["INBOX"];
                description = "IMAP folders to sync.";
              };
            };
            passwordFile = nullableString "Runtime path to the IMAP password.";
            exportDir = nullableString "Runtime directory containing exported mail.";
          };
        }));
        default = {};
        description = "Mail accounts managed by the Hub.";
      };
      bi = {
        enable = mkEnableOption "background mail business intelligence";
        llm = {
          enable = mkEnableOption "local LLM enrichment for mail";
          baseUrl = mkOption {
            type = types.str;
            default = "http://127.0.0.1:8013/v1";
            description = "Local OpenAI-compatible endpoint for mail enrichment.";
          };
          model = mkOption {
            type = types.str;
            default = "";
            description = "Model to use when LLM enrichment is enabled.";
          };
          maxBatchPerTick = positive 10 "Maximum messages enriched per pass (the daemon caps this at 100).";
          intervalSeconds = positive 120 "Seconds between enrichment passes.";
          skipJunk = mkOption {
            type = types.bool;
            default = true;
            description = "Do not send messages classified as junk to the local LLM.";
          };
        };
      };
    };

    calendar = {
      enable = mkEnableOption "Fleetix Hub calendar ingestion";
      syncIntervalSeconds = positive 300 "Seconds between calendar sync passes.";
      sources = mkOption {
        type = types.attrsOf (types.submodule ({name, ...}: {
          options = {
            owner = ownerOption;
            label = labelOption name;
            enabled = enabledOption;
            provider = mkOption {
              type = types.enum ["google_ics" "caldav" "google_api" "ics_url"];
              description = "Calendar sync protocol.";
            };
            visibility = mkOption {
              type = types.enum ["private" "shared"];
              default = "private";
              description = "Who can read the synced calendar.";
            };
            color = mkOption {
              type = types.str;
              default = "#4f8cff";
              description = "Calendar source display color.";
            };
            url = nullableString "Public, non-secret calendar URL only; written to the Nix store. For any private URL or token in a URL, use urlFile.";
            urlFile = nullableString "Runtime path to the calendar URL; takes precedence over url.";
            username = nullableString "Calendar login name.";
            passwordFile = nullableString "Runtime path to the calendar password.";
            serviceAccountFile = nullableString "Runtime path to Google service account JSON.";
            calendarId = nullableString "Google Calendar API calendar ID.";
          };
        }));
        default = {};
        description = "Calendar sources managed by the Hub.";
      };
    };

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

    fleetIssues = mkOption {
      type = types.nullOr (types.submodule {
        options = {
          companyId = mkOption {
            type = types.nonEmptyStr;
            description = "Paperclip company that receives fleet degradation issues.";
          };
          projectId = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Paperclip project for those issues.";
          };
          assigneeAgentId = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "Paperclip agent assigned to fix them.";
          };
        };
      });
      default = null;
      description = ''
        File each fleet condition the hub reports (pending-op approvals
        excepted) as a Paperclip issue, at most one per condition per UTC day.
        Null files none.
      '';
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

    secretInventoryFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Inventory JSON written by canix. Its directory is mounted read-only and must be outside protected home directories.";
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

    renewals = mkOption {
      type = types.listOf (types.submodule {
        options = {
          key = mkOption {type = types.str;};
          owner = mkOption {
            type = types.nullOr types.str;
            default = null;
          };
          title = mkOption {type = types.str;};
          procedure = mkOption {
            type = types.str;
            default = "";
          };
          urls = mkOption {
            type = types.listOf types.str;
            default = [];
            description = "Provider pages to open while renewing; must start with https://.";
          };
          account = mkOption {
            type = types.nullOr types.str;
            default = null;
          };
          cadenceDays = mkOption {
            type = types.nullOr types.ints.positive;
            default = null;
          };
          warnDays = mkOption {
            type = types.nullOr types.ints.unsigned;
            default = null;
          };
          dueAt = mkOption {
            type = types.nullOr types.str;
            default = null;
            description = "RFC 3339; null means unknown until the first renewal.";
          };
          expiryKind = mkOption {
            type = types.enum ["cadence" "never" "provider"];
            default = "cadence";
            description = ''
              Where the expiry comes from: the operator's cadence clock, an
              explicit never, or provider observations. A re-seed refreshes it;
              observations are never touched.
            '';
          };
        };
      });
      default = [];
      description = ''
        Credential and account renewal deadlines keyed stably by `key`. A
        re-seed refreshes title, procedure, URLs, cadence, account and expiry
        kind, but never moves the due date or the last renewal.
      '';
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
            type = types.enum ["operator" "agent" "procedure" "ingest" "member" "collector"];
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

    feeds = mkOption {
      type = types.attrsOf (types.submodule {
        options = {
          kind = mkOption {
            type = types.enum ["s3"];
            default = "s3";
            description = "How the document is fetched. `s3`: a SigV4-signed, path-style GetObject.";
          };
          endpoint = mkOption {
            type = types.str;
            example = "https://s3-west.nrp-nautilus.io";
            description = "S3 endpoint, `https://host[:port]` with no path.";
          };
          region = mkOption {
            type = types.str;
            example = "us-east-1";
            description = "SigV4 signing region.";
          };
          bucket = mkOption {type = types.str;};
          key = mkOption {
            type = types.str;
            description = "Object key of the status document.";
          };
          intervalSecs = mkOption {
            type = types.ints.between 30 86400;
            description = "Seconds between polls.";
          };
          staleAfterSecs = mkOption {
            type = types.ints.positive;
            description = "A document whose `metadata.timestamp` is older than this raises `monitor:<id>:feed`. At least `intervalSecs`.";
          };
          minStatus = mkOption {
            type = types.enum ["unknown" "warn" "fail"];
            default = "warn";
            description = "Lowest check status that notifies (fail = critical, warn = warning, unknown = info).";
          };
          credentials = {
            accessKeyId = mkOption {
              type = types.str;
              description = "Absolute path to the access key id file (an agenix path). Loaded as the systemd credential `feed-<id>-access-key-id`.";
            };
            secretAccessKey = mkOption {
              type = types.str;
              description = "Absolute path to the secret access key file (an agenix path). Loaded as the systemd credential `feed-<id>-secret-access-key`.";
            };
          };
        };
      });
      default = {};
      example = lib.literalExpression ''
        {
          syndb = {
            endpoint = "https://s3-west.nrp-nautilus.io";
            region = "us-east-1";
            bucket = "syndb-status";
            key = "report.json";
            intervalSecs = 300;
            staleAfterSecs = 2700;
            credentials.accessKeyId = config.age.secrets.syndb-status-read-key-id.path;
            credentials.secretAccessKey = config.age.secrets.syndb-status-read-secret.path;
          };
        }
      '';
      description = ''
        Status feeds the hub polls (`HUB_FEEDS_CONFIG`). Each check at or above
        `minStatus` becomes the notification `monitor:<id>:<check id>` and
        resolves when the check recovers; a stale or unreadable document raises
        `monitor:<id>:feed` and leaves the check rows as they were. Key material
        never enters the store or the environment: only the credential ids do.
        systemd copies credentials at unit start, so restart the unit after
        rotating a key.
      '';
    };

    intel = mkOption {
      type = types.attrsOf (pkgs.formats.json {}).type;
      default = {};
      description = "Intelligence source policies and principal grants. Rendered into the Nix store; do not include secrets.";
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
          assertion = cfg.secretInventoryFile == null || !lib.any (home: lib.hasPrefix home (toString cfg.secretInventoryFile)) ["/home/" "/root/" "/run/user/"];
          message = "fleetix-hub: secretInventoryFile must live outside /home, /root and /run/user (the unit runs with ProtectHome).";
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
          assertion = lib.all (id: builtins.match "[a-z0-9-]{1,32}" id != null) (lib.attrNames cfg.feeds);
          message = "fleetix-hub: feed ids must match [a-z0-9-]{1,32}.";
        }
        {
          assertion = lib.all (feed: feed.staleAfterSecs >= feed.intervalSecs) (lib.attrValues cfg.feeds);
          message = "fleetix-hub: a feed's staleAfterSecs must be at least its intervalSecs.";
        }
        {
          assertion = lib.all (feed: lib.all (p: lib.hasPrefix "/" p && !lib.hasPrefix "/nix/store/" p) [feed.credentials.accessKeyId feed.credentials.secretAccessKey]) (lib.attrValues cfg.feeds);
          message = "fleetix-hub: feed credentials must be absolute paths outside the Nix store (agenix paths).";
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

      systemd.tmpfiles.rules =
        ["d ${toString cfg.assetDir} 0750 ${runtimeUser} ${runtimeGroup} -"]
        ++ optional archiveEnabled "d ${cfg.mail.archiveRawDir} 0700 ${runtimeUser} ${runtimeGroup} -";

      systemd.services.fleetix-hub = {
        description = "fleetix-hub API and integration worker";
        wantedBy = ["multi-user.target"];
        after =
          ["network-online.target"]
          ++ optional cfg.database.createLocally "postgresql.service"
          ++ integrationUnits
          ++ passiveUnits;
        wants = ["network-online.target"] ++ integrationUnits;
        requires = optional cfg.database.createLocally "postgresql.service";
        partOf = optional cfg.database.createLocally "postgresql.service";
        onFailure = cfg.systemd.onFailure;
        unitConfig.StartLimitIntervalSec = lib.mkDefault 0;
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
          // optionalAttrs (cfg.fleetIssues != null) ({HUB_FLEET_ISSUE_COMPANY_ID = cfg.fleetIssues.companyId;}
            // optionalAttrs (cfg.fleetIssues.projectId != null) {HUB_FLEET_ISSUE_PROJECT_ID = cfg.fleetIssues.projectId;}
            // optionalAttrs (cfg.fleetIssues.assigneeAgentId != null) {HUB_FLEET_ISSUE_AGENT_ID = cfg.fleetIssues.assigneeAgentId;})
          // optionalAttrs (cfg.secretInventoryFile != null) {HUB_SECRET_INVENTORY_FILE = toString cfg.secretInventoryFile;}
          // optionalAttrs (feedsFile != null) {HUB_FEEDS_CONFIG = toString feedsFile;}
          // optionalAttrs (cfg.principalOwners != {}) {HUB_PRINCIPAL_OWNERS_CONFIG = toString (jsonFile "principal-owners" cfg.principalOwners);}
          // optionalAttrs mail.enable {HUB_MAIL_CONFIG = toString (jsonFile "mail" mailConfig);}
          // optionalAttrs calendar.enable {HUB_CALENDAR_CONFIG = toString (jsonFile "calendar" calendarConfig);}
          // optionalAttrs (cfg.intel != {}) {HUB_INTEL_CONFIG = toString (jsonFile "intel" cfg.intel);}
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
            RestartPreventExitStatus = 78;
            UMask = "0027";
            # The Postgres socket and the hub's HTTP listener need these.
            CapabilityBoundingSet = "";
            RestrictAddressFamilies = ["AF_UNIX" "AF_INET" "AF_INET6"];
            PrivateDevices = true;
            SystemCallFilter = ["@system-service" "~@privileged"];
            ReadWritePaths = [(toString cfg.assetDir)] ++ optional archiveEnabled cfg.mail.archiveRawDir;
          }
          // optionalAttrs (runtimeFiles != []) {
            ReadOnlyPaths = map optionalRuntimePath (lib.unique runtimeFiles);
          }
          // optionalAttrs (cfg.fleetStatusFile != null || cfg.secretInventoryFile != null) {
            # Bind the directory, not the file: atomic replacement gives the
            # snapshot a new inode. Read-only is all the hub needs.
            BindReadOnlyPaths = lib.optional (cfg.fleetStatusFile != null) (builtins.dirOf (toString cfg.fleetStatusFile)) ++ lib.optional (cfg.secretInventoryFile != null) (builtins.dirOf (toString cfg.secretInventoryFile));
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

    # Keep harbor-db's hard migration gate. A dependency-job failure does
    # not trigger the hub's Restart= or OnFailure=.
    (mkIf cfg.migrations.enable {
      systemd.services = {
        harbor-db-fleetix-hub.onFailure = cfg.systemd.onFailure;
        # Reuse harbor-db's successful-migration activation helper when the
        # local database starts, even if the hub has no active job to restart.
        harbor-db-fleetix-hub-activate.wantedBy = optional cfg.database.createLocally "postgresql.service";
      };

      services.harbor-db.projects.fleetix-hub = {
        enable = true;
        description = "fleetix-hub database migrations";
        runner = {
          executable = "${migrationLauncher}";
          args = ["migrate"];
          checkArgs = ["migrate" "--check"];
        };
        after = optional (!cfg.database.createLocally) "network-online.target";
        wants = optional (!cfg.database.createLocally) "network-online.target";
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
    })
  ]);
}
