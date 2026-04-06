{
  description = "Trueflow dev environment";
  nixConfig = { warn-dirty = false; };
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    beads = {
      url = "github:steveyegge/beads";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, beads, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];

        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain =
          pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        commonBuildInputs = with pkgs;
          [ pkg-config ]
          ++ lib.optionals stdenv.isDarwin [ apple-sdk libiconv ];

        rustPathRemapFlags = [
          "--remap-path-prefix=$NIX_BUILD_TOP=/build"
          "--remap-path-prefix=$PWD=/build/source"
        ];
        rustPathRemapFlagsString = pkgs.lib.concatStringsSep " " rustPathRemapFlags;

        mkTrueflowPackage = {
          rustPlatform,
          cargoBuildTarget ? null,
          doCheck ? false,
        }:
          rustPlatform.buildRustPackage ({
            pname = "trueflow";
            version = "0.1.0";
            src = ./trueflow;
            cargoLock = { lockFile = ./trueflow/Cargo.lock; };

            nativeBuildInputs = [ pkgs.pkg-config ];
            nativeCheckInputs = [ pkgs.gitMinimal ];
            buildInputs = commonBuildInputs;
            preConfigure = ''
              export RUSTFLAGS="''${RUSTFLAGS:+$RUSTFLAGS }${rustPathRemapFlagsString}"
            '';
            inherit doCheck;
          } // pkgs.lib.optionalAttrs (cargoBuildTarget != null) {
            CARGO_BUILD_TARGET = cargoBuildTarget;
          });

        # Keep ordinary package builds focused on packaging work. The cargo-based
        # local check path already runs tests, and the explicit *-with-tests
        # outputs below preserve the heavier buildRustPackage checkPhase when
        # needed.
        trueflow = mkTrueflowPackage {
          rustPlatform = rustPlatform;
          doCheck = false;
        };

        trueflowWithTests = mkTrueflowPackage {
          rustPlatform = rustPlatform;
          doCheck = true;
        };

        trueflowMusl = mkTrueflowPackage {
          rustPlatform = pkgs.pkgsStatic.rustPlatform;
          cargoBuildTarget = "${pkgs.pkgsStatic.stdenv.hostPlatform.config}";
          doCheck = false;
        };

        trueflowMuslWithTests = mkTrueflowPackage {
          rustPlatform = pkgs.pkgsStatic.rustPlatform;
          cargoBuildTarget = "${pkgs.pkgsStatic.stdenv.hostPlatform.config}";
          doCheck = true;
        };

        defaultPackage = if pkgs.stdenv.isDarwin then trueflow else trueflowMusl;
        defaultPackageWithTests = if pkgs.stdenv.isDarwin then trueflowWithTests else trueflowMuslWithTests;
      in {
        packages.default = defaultPackage;
        packages.native = trueflow;
        packages.musl = trueflowMusl;
        packages.static = trueflowMusl;
        packages.release = trueflowMusl;
        packages."native-with-tests" = trueflowWithTests;
        packages."default-with-tests" = defaultPackageWithTests;
        packages."musl-with-tests" = trueflowMuslWithTests;
        packages."static-with-tests" = trueflowMuslWithTests;
        packages."release-with-tests" = trueflowMuslWithTests;
        apps.default = flake-utils.lib.mkApp { drv = defaultPackage; };

        devShells.default = pkgs.mkShell {
          packages = with pkgs;
            [
              rustToolchain
              just

              # Cargo checks / lints / tools
              cargo-audit
              cargo-deny
              cargo-edit
              cargo-license
              cargo-llvm-cov
              cargo-nextest
              cargo-mutants
              cargo-pgo
              cargo-udeps
              cargo-watch
              gnupg
              trash-cli
              beads.packages.${system}.default
            ] ++ commonBuildInputs;

          shellHook = ''
            # Tells rust-analyzer where the stdlib sources are
            export RUST_SRC_PATH=${rustToolchain}/lib/rustlib/src/rust/library
            export TRUEFLOW_BIN=$PWD/trueflow/target/debug/trueflow
            export LC_COLLATE="''${LC_COLLATE:-C}"
          '';
        };
      });
}
