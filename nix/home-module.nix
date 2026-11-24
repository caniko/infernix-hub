{self}: args @ {
  config,
  lib,
  options,
  pkgs,
  ...
}: let
  cfg = config.programs.fleetixHub;
  inputs = args.inputs or (config._module.args.inputs or {});
  packages = self.packages.${pkgs.stdenv.hostPlatform.system};
  inherit (lib) mkEnableOption mkOption types;
  inherit (import (pkgs.path + "/nixos/lib/utils.nix") {inherit config lib pkgs;}) escapeSystemdExecArgs;
  # Token precedence: an explicit HUB_TOKEN wins and an empty one is refused
  # (never upgraded), then an explicit $HUB_TOKEN_FILE, then this user's tokenFile.
  tokenRun = ''
    if [ -n "''${HUB_TOKEN+set}" ]; then
      [ -n "$HUB_TOKEN" ] || { echo "HUB_TOKEN is set but empty" >&2; exit 1; }
    else
      default_token_file=${lib.escapeShellArg cfg.tokenFile}
      HUB_TOKEN=$(<"''${HUB_TOKEN_FILE:-$default_token_file}") || exit 1
      [ -n "$HUB_TOKEN" ] || { echo "the hub token file is empty" >&2; exit 1; }
      export HUB_TOKEN
    fi
  '';
  wrap = name: package: extra:
    pkgs.symlinkJoin {
      name = "${name}-configured";
      paths = [package];
      nativeBuildInputs = [pkgs.makeWrapper];
      postBuild = ''
        wrapProgram "$out/bin/${name}" \
          --set-default HUB_URL ${lib.escapeShellArg cfg.url} \
          --run ${lib.escapeShellArg tokenRun} \
          ${extra}
      '';
    };
  hubctl = wrap "hubctl" packages.hubctl "";
  desktop = wrap "fleetix-hub-desktop" packages.fleetix-hub-desktop (
    "--set-default HUB_PERSONAL_CALENDAR "
    + (
      if cfg.calendar.enable
      then "1"
      else "0"
    )
  );
  tray = wrap "fleetix-hub-tray" packages.fleetix-hub-tray "--set HUB_DESKTOP_COMMAND ${lib.escapeShellArg "${desktop}/bin/fleetix-hub-desktop"}";
  hasProxy = packages ? fleetix-agent-proxy;
  proxyPkg =
    if hasProxy
    then packages.fleetix-agent-proxy
    else null;
  provenanceOauthPkg =
    if cfg.provenanceOauthPackage != null
    then cfg.provenanceOauthPackage
    else if inputs ? nix-provenance
    then inputs.nix-provenance.packages.${pkgs.stdenv.hostPlatform.system}.provenance-oauth or null
    else null;
  proxy = lib.optionalAttrs hasProxy {
    wrapped = wrap "fleetix-agent-proxy" proxyPkg (
      lib.optionalString (provenanceOauthPkg != null)
      "--prefix PATH : ${lib.escapeShellArg (lib.makeBinPath [provenanceOauthPkg])}"
    );
  };
  proxyCfg = cfg.agentProxy;
  workerCfg = proxyCfg.workerListener;
  # The socket's leading %t is the only intentional systemd specifier.
  workerSocketArg = let
    escaped = escapeSystemdExecArgs [workerCfg.unixSocket];
  in
    if lib.hasPrefix ''"%%t/'' escaped
    then ''"%t/${lib.removePrefix ''"%%t/'' escaped}''
    else escaped;
  localProxy = cfg.enable && proxyCfg.enable && hasProxy;
  collectorCfg = cfg.collector;
  hasDeclarativeT3UsageSources =
    (options.programs ? t3code) && (options.programs.t3code ? usageLimitSources);
  t3codeEnabled = config.programs.t3code.enable or false;
  useDeclarativeT3Registration =
    hasDeclarativeT3UsageSources && t3codeEnabled && proxyCfg.registerT3Code;
  t3codeHome =
    if proxyCfg.t3codeHome != null
    then proxyCfg.t3codeHome
    else "${config.xdg.dataHome}/t3code-native";
  bridge = cfg.mail.protonBridge;
  useHydroxide = bridge.provider == "hydroxide";
  bridgePackage =
    if useHydroxide
    then pkgs.hydroxide
    else pkgs.protonmail-bridge;
  enabledEntries = lib.filterAttrs (_: entry: entry.enable);
  exports = enabledEntries cfg.mail.exportWatchers;
  localSyncs = enabledEntries cfg.mail.localBridgeSync;
  loopback = types.enum ["127.0.0.1" "::1" "localhost"];
  stringOption = description:
    mkOption {
      type = types.str;
      inherit description;
    };
  providerOption = default:
    mkOption {
      type = types.enum ["gmail" "protonmail" "tutanota" "imap" "export_dir"];
      inherit default;
      description = "Provider label when creating a client-push mail account.";
    };
  intervalOption = mkOption {
    type = types.str;
    default = "5m";
    description = "Systemd timer interval.";
  };
  clientJobs = prefix: entries: lib.mapAttrs' (name: lib.nameValuePair "fleetix-hub-mail-${prefix}-${name}") entries;
  jobService = description: condition: path: after: command: {
    Unit = {
      Description = description;
      ${condition} = lib.replaceStrings ["%"] ["%%"] path;
      After = after;
    };
    Service = {
      Type = "oneshot";
      ExecStart = escapeSystemdExecArgs (["${hubctl}/bin/hubctl" "mail"] ++ command);
      UMask = "0077";
    };
  };
  jobTimer = entry: {
    Timer = {
      OnStartupSec = "1m";
      OnUnitActiveSec = entry.interval;
    };
    Install.WantedBy = ["timers.target"];
  };
in {
  options.programs.fleetixHub = {
    enable = lib.mkEnableOption "Fleetix Hub desktop integration";
    url = lib.mkOption {
      type = lib.types.str;
      description = "Fleetix Hub API URL.";
    };
    tokenFile = lib.mkOption {
      type = lib.types.str;
      description = "Runtime path to the Fleetix Hub bearer token.";
    };
    provenanceOauthPackage = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "Optional OAuth helper for the local agent proxy; defaults to the nix-provenance input when available.";
    };
    calendar.enable = mkEnableOption "the personal schedule in Fleetix Hub Desktop (the wrapped hubctl also provides calendar commands)";
    mail = {
      enable = mkEnableOption "Fleetix Hub desktop mail integrations";
      protonBridge = {
        enable = mkEnableOption "a local Proton Mail bridge";
        provider = mkOption {
          type = types.enum ["hydroxide" "protonmail-bridge"];
          default = "hydroxide";
          description = "Bridge implementation. The official Proton bridge manages its own bind and ports.";
        };
        listenHost = mkOption {
          type = loopback;
          default = "127.0.0.1";
          description = "Loopback address for Hydroxide.";
        };
        imapPort = mkOption {
          type = types.port;
          default = 1143;
          description = "Hydroxide IMAP port.";
        };
        smtpPort = mkOption {
          type = types.port;
          default = 1025;
          description = "Hydroxide SMTP port.";
        };
      };
      tutaDesktop.enable = mkEnableOption "the official Tuta Desktop client";
      exportWatchers = mkOption {
        type = types.attrsOf (types.submodule ({name, ...}: {
          options = {
            enable = mkEnableOption "the ${name} mail export watcher";
            account = mkOption {
              type = types.str;
              default = name;
              description = "Hub account label or UUID.";
            };
            provider = providerOption "export_dir";
            address = stringOption "Email address for the imported account.";
            path = stringOption "Absolute directory containing exported .eml and .mbox files.";
            interval = intervalOption;
          };
        }));
        default = {};
        description = "Periodic imports of local mail exports.";
      };
      localBridgeSync = mkOption {
        type = types.attrsOf (types.submodule ({name, ...}: {
          options = {
            enable = mkEnableOption "the ${name} local IMAP bridge sync";
            account = mkOption {
              type = types.str;
              default = name;
              description = "Hub account label or UUID.";
            };
            provider = providerOption "protonmail";
            host = mkOption {
              type = loopback;
              default = bridge.listenHost;
              description = "Loopback address of the IMAP bridge.";
            };
            port = mkOption {
              type = types.port;
              default = bridge.imapPort;
              description = "IMAP bridge port.";
            };
            tls = mkOption {
              type = types.enum ["tls" "starttls" "plain"];
              default = "plain";
              description = "Security mode of the loopback IMAP bridge.";
            };
            username = stringOption "IMAP login name.";
            passwordFile = stringOption "Absolute runtime path to the IMAP password, outside the Nix store.";
            folders = mkOption {
              type = types.listOf types.str;
              default = ["INBOX"];
              description = "IMAP folders to import.";
            };
            interval = intervalOption;
          };
        }));
        default = {};
        description = "Periodic synchronization of loopback IMAP accounts.";
      };
    };
    agentProxy = {
      enable = lib.mkEnableOption "Spillway-compatible multi-account AI agent proxy";
      externalUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Existing agent proxy URL for clients, without starting a user proxy.";
      };
      effectiveUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        readOnly = true;
        internal = true;
        default =
          if localProxy
          then "http://${proxyCfg.bind}"
          else proxyCfg.externalUrl;
      };
      bind = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:8317";
        description = "Loopback bind address for fleetix-agent-proxy.";
      };
      workerListener = {
        enable = lib.mkEnableOption "the OAuth-only Codex worker socket";
        unixSocket = lib.mkOption {
          type = types.strMatching "(%t|/run)/[A-Za-z0-9._/-]+";
          default = "%t/fleetix-agent-proxy/worker.sock";
          description = "Private Unix socket for scoped workers under %t or /run, with no parent traversal.";
        };
        allowedModels = lib.mkOption {
          type = types.listOf (types.addCheck (types.strMatching "[A-Za-z0-9._:/-]+") (model: let
            lower = lib.toLower model;
          in
            !(lib.hasInfix "claude" lower || lib.hasInfix "anthropic" lower)));
          default = [];
          description = "Exact Codex model names permitted on the worker socket; required when enabled.";
        };
      };
      discoverLocal = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Discover local user-session Antigravity, Claude/Meridian, and OpenCode/Codex credentials.";
      };
      registerT3Code = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Register fleetix-agent-proxy into T3 Code's usageLimitSources on start.";
      };
      t3codeHome = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "T3 Code base directory (defaults to XDG_DATA_HOME/t3code-native).";
      };
      stateDir = lib.mkOption {
        type = lib.types.str;
        default = "${config.xdg.dataHome}/fleetix-agent-proxy";
        description = "Directory holding auto-generated client/management keys and proxy state.";
      };
      authDir = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Optional directory of CLIProxyAPI/Spillway credential JSON files.";
      };
    };
    collector = {
      enable = lib.mkEnableOption "User-session AI provider usage collector timer";
      interval = lib.mkOption {
        type = lib.types.str;
        default = "5m";
        description = "How often to collect local Antigravity, Claude, and OpenCode quotas and push to Fleetix Hub.";
      };
      tokenFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Optional scoped collector token file; defaults to programs.fleetixHub.tokenFile.";
      };
      meridianUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Optional loopback Meridian URL for Claude multi-profile quota observation.";
      };
      accountsFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Optional value-free Canix account catalogue JSON file.";
      };
    };
  };

  config = lib.mkMerge [
    {
      assertions = [
        {
          assertion = !(proxyCfg.enable && proxyCfg.externalUrl != null);
          message = "Fleetix Agent Proxy local and external modes are mutually exclusive.";
        }
        {
          assertion = !(lib.elem ".." (lib.splitString "/" workerCfg.unixSocket));
          message = "Fleetix Agent Proxy worker socket must not contain '..' path components.";
        }
        {
          assertion = !workerCfg.enable || workerCfg.allowedModels != [];
          message = "Fleetix Agent Proxy worker listener requires a non-empty model allowlist.";
        }
      ];
    }
    (lib.mkIf cfg.enable {
      home.packages = [hubctl desktop] ++ lib.optional hasProxy proxy.wrapped;
      systemd.user.services = lib.mkMerge [
        {
          fleetix-hub-tray = {
            Unit = {
              Description = "Fleetix Hub notification tray";
              PartOf = ["graphical-session.target"];
              After = ["graphical-session.target"];
            };
            Service = {
              ExecStart = "${tray}/bin/fleetix-hub-tray";
              Restart = "on-failure";
              RestartSec = 10;
              # The tray opens the console in its own `fleetix-hub-console` scope,
              # so stopping the tray leaves an open console alone.
            };
            Install.WantedBy = ["graphical-session.target"];
          };
        }
        (lib.mkIf (proxyCfg.enable && hasProxy) {
          fleetix-agent-proxy = {
            Unit = {
              Description = "Fleetix Spillway-compatible multi-account AI agent proxy";
              After =
                ["network-online.target"]
                ++ lib.optional (config.nix-provenance.antigravity.googleAuth.enable or false) "nix-provenance-antigravity-google-auth.service";
              Before = ["t3code.service"];
            };
            Service = {
              ExecStart =
                escapeSystemdExecArgs (
                  [
                    "${proxy.wrapped}/bin/fleetix-agent-proxy"
                    "serve"
                    "--bind"
                    proxyCfg.bind
                    "--state-dir"
                    proxyCfg.stateDir
                    "--hub-url"
                    cfg.url
                    "--hub-token-file"
                    (
                      if collectorCfg.tokenFile != null
                      then collectorCfg.tokenFile
                      else cfg.tokenFile
                    )
                  ]
                  ++ lib.optionals (proxyCfg.authDir != null) ["--auth-dir" proxyCfg.authDir]
                  ++ lib.optional proxyCfg.discoverLocal "--discover-local"
                  ++ lib.optionals (proxyCfg.registerT3Code && !useDeclarativeT3Registration) [
                    "--register-t3code"
                    "--t3code-home"
                    t3codeHome
                  ]
                )
                + lib.optionalString workerCfg.enable (
                  " \"--worker-unix-socket\" ${workerSocketArg} "
                  + escapeSystemdExecArgs (map (model: "--worker-allowed-models=${model}") workerCfg.allowedModels)
                );
              Restart = "on-failure";
              RestartSec = 5;
              UMask = "0077";
            };
            Install.WantedBy = ["default.target"];
          };
        })
        (lib.mkIf (collectorCfg.enable && hasProxy) {
          fleetix-hub-collector = {
            Unit = {
              Description = "Collect user-session AI provider quotas and sync to Fleetix Hub";
              After = ["network-online.target"] ++ lib.optional proxyCfg.enable "fleetix-agent-proxy.service";
            };
            Service = {
              Type = "oneshot";
              Environment = lib.optional (collectorCfg.tokenFile != null) "HUB_TOKEN_FILE=${collectorCfg.tokenFile}";
              ExecStart = lib.concatStringsSep " " (
                [
                  "${hubctl}/bin/hubctl"
                  "providers"
                  "collect"
                  "--claim-ops"
                ]
                ++ lib.optionals (collectorCfg.meridianUrl != null) ["--meridian-url" (lib.escapeShellArg collectorCfg.meridianUrl)]
                ++ lib.optionals (collectorCfg.accountsFile != null) ["--accounts-file" (lib.escapeShellArg (toString collectorCfg.accountsFile))]
                ++ lib.optionals proxyCfg.enable [
                  "--proxy-url"
                  (lib.escapeShellArg "http://${proxyCfg.bind}")
                  "--proxy-management-key-file"
                  (lib.escapeShellArg "${proxyCfg.stateDir}/management-key")
                ]
              );
              UMask = "0077";
            };
          };
        })
      ];
      systemd.user.timers.fleetix-hub-collector = lib.mkIf (collectorCfg.enable && hasProxy) {
        Unit.Description = "Periodic AI provider usage collection for Fleetix Hub";
        Timer = {
          OnBootSec = "1m";
          OnUnitActiveSec = collectorCfg.interval;
          RandomizedDelaySec = "30s";
        };
        Install.WantedBy = ["timers.target"];
      };
      # The console's palette follows the theme broker's semantic roles; the
      # hub reads `$XDG_CONFIG_HOME/fleetix-hub/theme.json` (apps/desktop
      # `theme.rs` PALETTE keys) and keeps its built-in value for any key
      # left out.
      xdg.configFile."fleetix-hub/theme.json" = lib.mkIf ((options ? themeBroker) && (config.themeBroker.enable or false)) {
        text = let
          inherit (config.themeBroker.selected.roles) ui status;
        in
          builtins.toJSON (lib.mapAttrs (_: color: color.withHashtag) {
            bg = ui.background;
            panel = ui.backgroundAlt;
            card = ui.surface;
            elevated = ui.surfaceAlt;
            inherit (ui) border;
            text = ui.foreground;
            textMuted = ui.foregroundMuted;
            inherit (ui) accent;
            inherit (ui) focus;
            ok = status.success;
            warn = status.warning;
            bad = status.error;
            inherit (status) info;
          });
      };
    })
    (lib.mkIf (cfg.enable && cfg.mail.enable) {
      assertions = [
        {
          assertion = lib.all (name: builtins.match "[A-Za-z0-9_.-]+" name != null) (lib.attrNames exports ++ lib.attrNames localSyncs);
          message = "Fleetix Hub mail watcher and bridge sync names must be safe systemd unit suffixes.";
        }
        {
          assertion = lib.all (entry: lib.hasPrefix "/" entry.path) (lib.attrValues exports);
          message = "Fleetix Hub mail export watchers need an absolute directory.";
        }
        {
          assertion = lib.all (entry: lib.hasPrefix "/" entry.passwordFile && !lib.hasPrefix "/nix/store/" entry.passwordFile) (lib.attrValues localSyncs);
          message = "Fleetix Hub mail bridge sync needs an absolute runtime password path outside the Nix store.";
        }
      ];
      home.packages = lib.optional bridge.enable bridgePackage ++ lib.optional cfg.mail.tutaDesktop.enable pkgs.tutanota-desktop;
      systemd.user.services =
        lib.optionalAttrs bridge.enable {
          fleetix-hub-proton-bridge = {
            Unit =
              {Description = "Fleetix Hub local Proton Mail bridge";}
              // lib.optionalAttrs useHydroxide {
                ConditionPathExists = "%h/.config/hydroxide/auth.json";
              };
            Service = {
              ExecStart =
                if useHydroxide
                then "${pkgs.hydroxide}/bin/hydroxide -imap-host ${bridge.listenHost} -imap-port ${toString bridge.imapPort} -smtp-host ${bridge.listenHost} -smtp-port ${toString bridge.smtpPort} serve"
                else "${pkgs.protonmail-bridge}/bin/protonmail-bridge --noninteractive";
              Restart = "on-failure";
              RestartSec = 10;
              UMask = "0077";
            };
            Install.WantedBy = ["default.target"];
          };
        }
        // clientJobs "import" (lib.mapAttrs (_: entry:
          jobService "Import Fleetix Hub mail export" "ConditionDirectoryNotEmpty" entry.path [] [
            "import"
            "--account"
            entry.account
            "--provider"
            entry.provider
            "--address"
            entry.address
            "--"
            entry.path
          ])
        exports)
        // clientJobs "bridge-sync" (lib.mapAttrs (_: entry:
          jobService "Sync local IMAP mail to Fleetix Hub" "ConditionPathExists" entry.passwordFile (lib.optional bridge.enable "fleetix-hub-proton-bridge.service") [
            "sync-imap"
            "--account"
            entry.account
            "--provider"
            entry.provider
            "--host"
            entry.host
            "--port"
            entry.port
            "--tls"
            entry.tls
            "--username"
            entry.username
            "--password-file"
            entry.passwordFile
            "--folders"
            (lib.concatStringsSep "," entry.folders)
          ])
        localSyncs);
      systemd.user.timers =
        clientJobs "import" (lib.mapAttrs (_: jobTimer) exports)
        // clientJobs "bridge-sync" (lib.mapAttrs (_: jobTimer) localSyncs);
      systemd.user.tmpfiles.rules =
        lib.mapAttrsToList (_: entry: ''d ${builtins.toJSON (lib.replaceStrings ["%"] ["%%"] entry.path)} 0700 - - -'')
        exports;
    })
    (lib.mkIf (localProxy && t3codeEnabled) {
      systemd.user.services.t3code.Unit = {
        Wants = ["fleetix-agent-proxy.service"];
        After = ["fleetix-agent-proxy.service"];
      };
    })
    (lib.optionalAttrs (lib.hasAttrByPath ["programs" "t3code" "server" "environment"] options) {
      programs.t3code.server.environment = lib.mkIf (t3codeEnabled && proxyCfg.effectiveUrl != null) {
        T3_ANTIGRAVITY_TRUSTED_POOL_PROXY_URL = proxyCfg.effectiveUrl;
        CLOUD_CODE_URL = proxyCfg.effectiveUrl;
      };
    })
    (lib.optionalAttrs hasDeclarativeT3UsageSources {
      programs.t3code.usageLimitSources = lib.mkIf (localProxy && useDeclarativeT3Registration) {
        fleetix-proxy = {
          enable = true;
          kind = "cliproxy";
          label = "Fleetix Agent Proxy";
          url = "http://${proxyCfg.bind}";
          managementKeyFile = "${proxyCfg.stateDir}/management-key";
          optional = true;
        };
      };
    })
  ];
}
