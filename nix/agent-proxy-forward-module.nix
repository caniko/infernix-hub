{
  config,
  lib,
  utils,
  ...
}: let
  cfg = config.services.fleetixAgentProxyForward;
  listen = "${cfg.listenAddress}:${toString cfg.port}";
  target = "${cfg.targetHost}:${toString cfg.targetPort}";
in {
  options.services.fleetixAgentProxyForward = {
    enable = lib.mkEnableOption "loopback forwarding to a remote Fleetix Agent Proxy listener";

    listenAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = "Loopback address to expose to local proxy clients.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8317;
      description = "Local TCP port for proxy clients.";
    };

    targetHost = lib.mkOption {
      type = lib.types.str;
      description = "IP address or host of the remote Fleetix Agent Proxy listener.";
    };

    targetPort = lib.mkOption {
      type = lib.types.port;
      default = 8317;
      description = "TCP port of the remote Fleetix Agent Proxy listener.";
    };

    guardInterface = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "wg-home";
      description = ''
        Optional network interface name (such as a WireGuard interface) to
        enforce via an nftables output drop rule so cleartext proxy traffic
        cannot fall back onto another route. Requires `targetHost` to be a
        literal IPv4 address when set.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          cfg.guardInterface
          == null
          || builtins.match "[0-9]{1,3}(\\.[0-9]{1,3}){3}" cfg.targetHost != null;
        message = "services.fleetixAgentProxyForward.targetHost must be a literal IPv4 address when guardInterface is set.";
      }
    ];

    systemd.sockets.fleetix-agent-proxy-forward = {
      wantedBy = ["sockets.target"];
      requires = lib.optionals (cfg.guardInterface != null) ["nftables.service"];
      after = lib.optionals (cfg.guardInterface != null) ["nftables.service"];
      listenStreams = [listen];
    };

    systemd.services.fleetix-agent-proxy-forward.serviceConfig = {
      ExecStart = utils.escapeSystemdExecArgs [
        "${config.systemd.package}/lib/systemd/systemd-socket-proxyd"
        target
      ];
      DynamicUser = true;
      NoNewPrivileges = true;
      PrivateDevices = true;
      ProtectHome = true;
      ProtectSystem = "strict";
      RestrictAddressFamilies = ["AF_UNIX" "AF_INET" "AF_INET6"];
    };

    # Refuse cleartext fallback if the route ever leaves the guarded interface.
    networking.nftables = lib.mkIf (cfg.guardInterface != null) {
      enable = true;
      tables.fleetix-agent-proxy = {
        family = "inet";
        content = ''
          chain output {
            type filter hook output priority filter;
            oifname != "${cfg.guardInterface}" ip daddr ${cfg.targetHost} tcp dport ${toString cfg.targetPort} drop
          }
        '';
      };
    };
  };
}
