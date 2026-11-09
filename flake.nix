{
  description = "infernix-hub: local-first content operations hub";

  inputs = {
    # One nixpkgs for the flake, harbor-rs, harbor-db, and infernix.
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    # Toolchain, crane, sccache policy, dev shells, and the formatter module.
    # Update the revision deliberately: the build-cache namespace and the
    # wrapper contract are part of the deployment envelope.
    harbor-rs = {
      url = "git+https://github.com/caniko/harbor-rs.git?ref=trunk&rev=ed89d0b13fc61dd1b2217bf4bba97f32cec27ba7";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # Migration ordering, grants, and the `migrate --check` gate.
    harbor-db = {
      url = "github:caniko/harbor-db/febc9f30022e42e2a9b8cb4e64d71ba28c79b7b1";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.harbor-rs.follows = "harbor-rs";
      inputs.crane.follows = "crane";
    };

    # ComfyUI, llama-swap, and load-balancer options the NixOS module reads.
    # The module only uses infernix when a host imports it, but the input is
    # direct so the module-eval checks exercise the real option tree.
    infernix = {
      url = "git+https://github.com/caniko/infernix.git?ref=trunk";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.harbor-rs.follows = "harbor-rs";
    };

    rust-overlay.follows = "harbor-rs/rust-overlay";
    crane.follows = "harbor-rs/crane";
    treefmt-nix.follows = "harbor-rs/treefmt-nix";
  };

  outputs = {
    self,
    nixpkgs,
    harbor-rs,
    harbor-db,
    infernix,
    rust-overlay,
    treefmt-nix,
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

      # Same compiler-cache contract as the rest of the fleet: the wrapper and
      # namespace come from harbor-rs, so hubs share hits with sibling Rust
      # projects on the same host.
      sccachePackage = harbor-rs.packages.${system}.sccache;
      buildCachePolicy = harbor-rs.lib.mkBuildCachePolicy {
        inherit pkgs sccachePackage;
        buildPackageSet = pkgs.buildPackages // {sccache = sccachePackage;};
        cacheRoot = null;
        namespaceScope = "canix-rust";
        namespaceGeneration = 5;
      };

      treefmt = treefmt-nix.lib.evalModule pkgs {
        imports = [harbor-rs.treefmtModules.rust ./nix/treefmt.nix];
        projectRootFile = "flake.nix";
        programs.rustfmt.package = toolchain.rustToolchain;
      };
      treefmtWrapper = treefmt.config.build.wrapper;

      packages = import ./nix/packages.nix {
        inherit pkgs craneLib;
        cacheRust = buildCachePolicy.withRustCache;
        treefmtCheck = treefmt.config.build.check self;
      };

      # Make `hubctl` resolve to this checkout's build, never a stale PATH one.
      cliTools = harbor-rs.lib.mkProjectCliShellTools {
        inherit pkgs;
        package = packages.hubctl;
        commandName = "hubctl";
        hint = "hubctl reads HUB_URL/HUB_TOKEN; use `cargo run -p hubctl` while iterating.";
      };

      shells = harbor-rs.lib.mkDevShells {
        inherit pkgs cross craneLib;
        pkgConfigDeps = packages.desktopBuildInputs;
        packages =
          packages.desktopBuildInputs
          ++ cliTools.packages
          ++ (with pkgs; [pkg-config postgresql_16 cargo-nextest treefmtWrapper]);
        extraEnv = {
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath packages.desktopBuildInputs;
          # WebKitGTK's DMA-BUF renderer fails on some GPU stacks.
          WEBKIT_DISABLE_DMABUF_RENDERER = "1";
        };
        extraShellHook = cliTools.shellHook;
      };

      # The infernix input only evaluates cleanly on x86_64-linux.
      moduleChecks =
        nixpkgs.lib.optionalAttrs (system == "x86_64-linux")
        (import ./nix/module-eval-check.nix {
          inherit pkgs;
          module = self.nixosModules.default;
          infernixModule = infernix.nixosModules.default;
        });
    in {
      inherit packages shells treefmtWrapper;
      checks = packages.checks // moduleChecks;
    });
  in {
    packages = forAllSystems (system: {
      inherit (perSystem.${system}.packages) infernix-hubd hubctl infernix-hub-desktop;
      default = perSystem.${system}.packages.infernix-hubd;
    });

    checks = forAllSystems (system: perSystem.${system}.checks);

    devShells = forAllSystems (system: perSystem.${system}.shells);

    formatter = forAllSystems (system: perSystem.${system}.treefmtWrapper);

    nixosModules.default = import ./nix/module.nix {inherit self harbor-rs harbor-db;};
    nixosModules.infernix-hub = self.nixosModules.default;
  };
}
