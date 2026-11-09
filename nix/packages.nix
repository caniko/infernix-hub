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
      ../migrations
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
    pname = "infernix-hub";
    inherit version src;
    strictDeps = true;
    nativeBuildInputs = [pkgs.pkg-config];
  };

  nativeArgs =
    common
    // {
      # default-members already exclude the desktop crate; spell it out so a
      # future member cannot silently pull webkit into the server graph.
      cargoExtraArgs = "--locked --workspace --exclude infernix-hub-desktop";
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
            description = "infernix-hub ${name}";
            mainProgram = name;
          };
        });
    };

  server = mkBin "infernix-hubd";

  desktopArgs =
    common
    // {
      pname = "infernix-hub-desktop";
      cargoExtraArgs = "--locked -p infernix-hub-desktop";
      buildInputs = desktopBuildInputs;
    };
in {
  inherit desktopBuildInputs;

  infernix-hubd = server.overrideAttrs (old: {
    # The module points HUB_REGISTRY_DIR at this copy; the server reads the
    # ComfyUI workflow JSON from it at startup.
    postInstall =
      (old.postInstall or "")
      + ''
        install -dm755 "$out/share/infernix-hub"
        cp -r registry "$out/share/infernix-hub/registry"
      '';
  });

  hubctl = mkBin "hubctl";

  infernix-hub-desktop = cacheRust {
    package = craneLib.buildPackage (desktopArgs
      // {
        cargoArtifacts = cacheRust {package = craneLib.buildDepsOnly desktopArgs;};
        doCheck = false;
        nativeBuildInputs = [pkgs.pkg-config pkgs.wrapGAppsHook3];
        meta = {
          description = "infernix-hub operator console";
          mainProgram = "infernix-hub-desktop";
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
