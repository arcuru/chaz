{
  description = "chaz - AI agent orchestrator for Matrix.";

  nixConfig = {
    extra-substituters = ["https://cache.eidetica.dev"];
    extra-trusted-public-keys = ["cache.eidetica.dev-1:eND5gRJlbnool3ZLCWT2H8kkygWS8JcsU76HYXbWPBI="];
  };

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-analyzer-src.follows = "";
    };

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {flake-parts, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      imports = [
        inputs.flake-parts.flakeModules.easyOverlay
        inputs.treefmt-nix.flakeModule
      ];

      flake = {
        homeManagerModules.default = import ./nix/home-manager.nix;
      };

      systems = [
        "aarch64-linux"
        "x86_64-linux"
        "aarch64-darwin"
      ];

      perSystem = {
        config,
        system,
        pkgs,
        lib,
        ...
      }: let
        # Pin to Rust 1.93.0 to avoid rustc regression (rust-lang/rust#152942)
        # that causes matrix-sdk 0.16 to hit query depth limits on 1.94+
        fenixStable = inputs.fenix.packages.${system}.toolchainOf {
          channel = "1.93.0";
          sha256 = "sha256-vra6TkHITpwRyA5oBKAHSX0Mi6CBDNQD+ryPSpxFsfg=";
        };
        rustSrc = fenixStable.rust-src;
        toolChain = fenixStable.completeToolchain;

        # Use the toolchain with the crane helper functions
        craneLib = (inputs.crane.mkLib pkgs).overrideToolchain toolChain;

        # Source filtering — only Rust-relevant files
        src = craneLib.cleanCargoSource (craneLib.path ./.);

        # Common arguments
        commonArgs = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          buildInputs = with pkgs; [
            openssl
            sqlite
          ];
          # Test binaries link openssl dynamically; checkPhase runs them in the
          # build sandbox where they need libssl on LD_LIBRARY_PATH at runtime.
          LD_LIBRARY_PATH = lib.makeLibraryPath [pkgs.openssl];
        };

        # Test-only extras: subprocess tests spawn python3, and tests that
        # touch $HOME need it to point at a writable path inside the sandbox.
        testArgs =
          buildArgs
          // {
            nativeBuildInputs = buildArgs.nativeBuildInputs or [] ++ [pkgs.python3];
            preCheck = ''
              export HOME=$TMPDIR
            '';
          };

        # Build only cargo dependencies for caching
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Args with cached artifacts
        buildArgs = commonArgs // {inherit cargoArtifacts;};

        # Build the binary. Tests run separately via the `test` check.
        chaz-unwrapped = craneLib.buildPackage (buildArgs // {doCheck = false;});

        # Wrap chaz to include aichat in PATH
        chaz =
          pkgs.runCommand chaz-unwrapped.name {
            inherit (chaz-unwrapped) pname version;
            nativeBuildInputs = [pkgs.makeWrapper];
          } ''
            mkdir -p $out/bin
            cp ${chaz-unwrapped}/bin/chaz $out/bin
            wrapProgram $out/bin/chaz \
              --prefix PATH : ${lib.makeBinPath [pkgs.aichat]}
          '';

        # Lint packages
        chaz-clippy = craneLib.cargoClippy (buildArgs
          // {
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

        chaz-doc = craneLib.cargoDoc buildArgs;

        chaz-deny = craneLib.mkCargoDerivation (buildArgs
          // {
            pnameSuffix = "-deny";
            buildPhaseCargoCommand = "cargo deny check --config .config/deny.toml";
            nativeBuildInputs = (buildArgs.nativeBuildInputs or []) ++ [pkgs.cargo-deny];
            # cargo-deny needs the full source for config
            src = ./.;
          });

        # Helper to create aggregate packages
        mkAggregate = name: packages:
          pkgs.symlinkJoin {
            inherit name;
            paths = builtins.attrValues packages;
          };

        lintDefaults = {
          inherit chaz-clippy;
        };

        lintAll =
          lintDefaults
          // {
            inherit chaz-deny;
          };
      in {
        # Hierarchical package structure via legacyPackages
        legacyPackages = {
          # Lint group
          lint =
            lintAll
            // {
              default = mkAggregate "lint" lintDefaults;
              all = mkAggregate "lint-all" lintAll;
            };

          # Test group
          test = {
            default = craneLib.cargoTest testArgs;
          };

          # Doc group
          doc = {
            default = chaz-doc;
          };

          # Main package
          chaz = {
            bin = chaz;
            unwrapped = chaz-unwrapped;
          };

          default = chaz;
        };

        packages = {
          inherit chaz chaz-unwrapped;
          default = chaz;
        };

        # CI checks — run during `nix flake check`
        checks = {
          build = chaz-unwrapped;
          test = craneLib.cargoTest testArgs;
          lint = mkAggregate "lint" lintDefaults;
          doc = chaz-doc;
        };

        # Formatting via treefmt
        treefmt = {
          projectRootFile = "flake.nix";
          programs = {
            alejandra.enable = true;
            prettier.enable = true;
            rustfmt.enable = true;
          };
        };

        apps = {
          default = {
            type = "app";
            program = "${chaz}/bin/chaz";
          };
        };

        # Overlay for downstream consumers
        overlayAttrs = {
          inherit (config.packages) chaz;
        };

        devShells.default = pkgs.mkShell {
          name = "chaz";
          shellHook = ''
            echo ---------------------
            just --list
            echo ---------------------
          '';

          inputsFrom = [
            chaz-unwrapped
            chaz-clippy
          ];

          nativeBuildInputs = with pkgs; [
            alejandra
            cargo-deny
            cargo-nextest
            deadnix
            git-cliff
            just
            mdbook
            prettier
            nix-fast-build
            statix
            config.treefmt.build.wrapper
          ];

          RUST_SRC_PATH = "${rustSrc}/lib/rustlib/src/rust/library";

          # Ensure dynamically-linked test binaries can find openssl at runtime.
          # inputsFrom provides headers/pkg-config for compilation but not LD_LIBRARY_PATH.
          LD_LIBRARY_PATH = lib.makeLibraryPath [pkgs.openssl];
        };
      };
    };
}
