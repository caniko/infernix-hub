# Crane builds. The native server/CLI graph is built separately from the
# desktop UI so the webkit stack never enters the service closure.
{
  pkgs,
  craneLib,
  cacheRust,
  treefmtCheck,
}: let
  inherit (pkgs) lib;

  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../apps
      ../crates
      ../registry
    ];
  };

  # Runtime dependencies of the Dioxus 0.8 desktop shell (wry/WebKitGTK).
  desktopBuildInputs = with pkgs; [
    webkitgtk_4_1
    gtk3
    libsoup_3
    glib
    cairo
    pango
    gdk-pixbuf
    atk
    openssl
    xdotool
  ];

  common = {
    pname = "fleetix-hub";
    inherit version src;
    strictDeps = true;
    nativeBuildInputs = [pkgs.pkg-config];
  };

  nativeArgs =
    common
    // {
      # default-members already exclude the desktop crate; spell it out so a
      # future member cannot silently pull webkit into the server graph.
      cargoExtraArgs = "--locked --workspace --exclude fleetix-hub-desktop";
    };
  nativeArtifacts = cacheRust {package = craneLib.buildDepsOnly nativeArgs;};

  mkBin = name:
    cacheRust {
      package = craneLib.buildPackage (nativeArgs
        // {
          pname = name;
          cargoArtifacts = nativeArtifacts;
          cargoExtraArgs = "--locked -p ${name}";
          doCheck = false;
          meta = {
            description = "fleetix-hub ${name}";
            mainProgram = name;
            license = pkgs.lib.licenses.eupl12;
          };
        });
    };

  server = mkBin "fleetix-hubd";

  desktopArgs =
    common
    // {
      pname = "fleetix-hub-desktop";
      cargoExtraArgs = "--locked -p fleetix-hub-desktop";
      buildInputs = desktopBuildInputs;
    };
in {
  inherit desktopBuildInputs;

  fleetix-hubd = server.overrideAttrs (old: {
    # The module points HUB_REGISTRY_DIR at this copy; the server reads the
    # ComfyUI workflow JSON from it at startup.
    postInstall =
      (old.postInstall or "")
      + ''
        install -dm755 "$out/share/fleetix-hub"
        cp -r registry "$out/share/fleetix-hub/registry"
      '';
  });

  hubctl = mkBin "hubctl";

  # ksni and zbus only; no webkit, so it stays in the native graph above.
  fleetix-hub-tray = mkBin "fleetix-hub-tray";

  # Spillway-compatible multi-account AI agent proxy (Anthropic/OpenAI/Antigravity + T3 Code /v0/management + /v8/management).
  fleetix-agent-proxy = mkBin "fleetix-agent-proxy";

  fleetix-hub-desktop = cacheRust {
    package = craneLib.buildPackage (desktopArgs
      // {
        cargoArtifacts = cacheRust {package = craneLib.buildDepsOnly desktopArgs;};
        doCheck = false;
        nativeBuildInputs = [pkgs.pkg-config pkgs.wrapGAppsHook3 pkgs.copyDesktopItems];
        desktopItems = [
          (pkgs.makeDesktopItem {
            name = "fleetix-hub-desktop";
            desktopName = "Fleetix Hub";
            genericName = "Fleet operations console";
            comment = "Notifications, fleet health and content operations";
            exec = "fleetix-hub-desktop";
            icon = "fleetix-hub-desktop";
            terminal = false;
            categories = ["Utility" "Monitor"];
          })
        ];
        # The launcher icon reuses the console's own nav asset; the tray starts
        # this same binary with `--workspace notifications`.
        postInstall = ''
          install -Dm644 apps/desktop/assets/icons/command-center.svg \
            "$out/share/icons/hicolor/scalable/apps/fleetix-hub-desktop.svg"
        '';
        meta = {
          description = "fleetix-hub operator console";
          mainProgram = "fleetix-hub-desktop";
          license = pkgs.lib.licenses.eupl12;
        };
      });
  };

  checks = {
    # Native checks only; DB-backed tests self-skip without
    # HUB_TEST_DATABASE_URL.
    clippy = cacheRust {
      package = craneLib.cargoClippy (nativeArgs
        // {
          cargoArtifacts = nativeArtifacts;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        });
    };
    nextest = cacheRust {
      package = craneLib.cargoNextest (nativeArgs // {cargoArtifacts = nativeArtifacts;});
    };
    # treefmt owns formatting (rustfmt + alejandra); `cargo fmt` would skip
    # include!()d sources.
    fmt = treefmtCheck;
  };
}
