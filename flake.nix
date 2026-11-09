{
  description = "infernix-hub: local-first content operations hub";

  inputs = {
    harbor-rs.url = "git+https://github.com/caniko/harbor-rs.git?ref=trunk&rev=ed89d0b13fc61dd1b2217bf4bba97f32cec27ba7";
    nixpkgs.follows = "harbor-rs/nixpkgs";
    rust-overlay.follows = "harbor-rs/rust-overlay";
    crane.follows = "harbor-rs/crane";
  };

  outputs = {
    self,
    nixpkgs,
    harbor-rs,
    rust-overlay,
    ...
  }: let
    systems = ["x86_64-linux" "aarch64-linux"];
    forAllSystems = nixpkgs.lib.genAttrs systems;
    perSystem = forAllSystems (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      toolchain = harbor-rs.lib.mkToolchain {inherit pkgs;};
      cross = harbor-rs.lib.mkCross {inherit pkgs system;};
      inherit (toolchain) craneLib;
      packages = import ./nix/packages.nix {inherit pkgs craneLib;};
    in {
      inherit packages;
      devShell = harbor-rs.lib.mkDevShells {
        inherit pkgs cross craneLib;
        pkgConfigDeps = packages.desktopBuildInputs;
        packages =
          packages.desktopBuildInputs
          ++ (with pkgs; [pkg-config postgresql_16 sqlx-cli cargo-nextest]);
        extraEnv = {
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath packages.desktopBuildInputs;
          # WebKitGTK DMA-BUF renderer fails on some GPU stacks; fall back safely.
          WEBKIT_DISABLE_DMABUF_RENDERER = "1";
        };
      };
    });
  in {
    packages = forAllSystems (s: {
      inherit (perSystem.${s}.packages) infernix-hubd hubctl infernix-hub-desktop;
      default = perSystem.${s}.packages.infernix-hubd;
    });
    checks = forAllSystems (s: perSystem.${s}.packages.checks);
    devShells = forAllSystems (s: perSystem.${s}.devShell);
    nixosModules.default = import ./nix/module.nix self;
  };
}
