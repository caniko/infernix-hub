# Crane builds. Native server/CLI build separately from the desktop UI so the
# webkit stack never enters the service closure.
{
  pkgs,
  craneLib,
}: let
  inherit (pkgs) lib;
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
    version = "0.1.0";
    inherit src;
    strictDeps = true;
    nativeBuildInputs = [pkgs.pkg-config];
  };
  nativeArgs = common // {cargoExtraArgs = "--locked --workspace --exclude infernix-hub-desktop";};
  nativeArtifacts = craneLib.buildDepsOnly nativeArgs;
  bin = name:
    craneLib.buildPackage (nativeArgs
      // {
        pname = name;
        cargoArtifacts = nativeArtifacts;
        cargoExtraArgs = "--locked -p ${name}";
        doCheck = false;
        meta.mainProgram = name;
      });
  desktopArgs =
    common
    // {
      pname = "infernix-hub-desktop";
      cargoExtraArgs = "--locked -p infernix-hub-desktop";
      buildInputs = desktopBuildInputs;
    };
in {
  inherit desktopBuildInputs;
  infernix-hubd = bin "infernix-hubd";
  hubctl = bin "hubctl";
  infernix-hub-desktop = craneLib.buildPackage (desktopArgs
    // {
      cargoArtifacts = craneLib.buildDepsOnly desktopArgs;
      doCheck = false;
      nativeBuildInputs = [pkgs.pkg-config pkgs.wrapGAppsHook3];
      meta.mainProgram = "infernix-hub-desktop";
    });
  checks = {
    # Native checks only; DB-backed tests self-skip without HUB_TEST_DATABASE_URL.
    clippy = craneLib.cargoClippy (nativeArgs
      // {
        cargoArtifacts = nativeArtifacts;
        cargoClippyExtraArgs = "--all-targets -- --deny warnings";
      });
    nextest = craneLib.cargoNextest (nativeArgs // {cargoArtifacts = nativeArtifacts;});
    fmt = craneLib.cargoFmt {inherit src;};
  };
}
