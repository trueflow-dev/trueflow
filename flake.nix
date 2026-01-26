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

        trueflow = rustPlatform.buildRustPackage {
          pname = "trueflow";
          version = "0.1.0";
          src = ./trueflow;
          cargoLock = { lockFile = ./trueflow/Cargo.lock; };

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = commonBuildInputs;
        };

        trueflowMusl = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
          pname = "trueflow";
          version = "0.1.0";
          src = ./trueflow;
          cargoLock = { lockFile = ./trueflow/Cargo.lock; };

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = commonBuildInputs;
          CARGO_BUILD_TARGET = "${pkgs.pkgsStatic.stdenv.hostPlatform.config}";
        };
      in {
        packages.default = trueflow;
        packages.musl = trueflowMusl;
        apps.default = flake-utils.lib.mkApp { drv = trueflow; };

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
