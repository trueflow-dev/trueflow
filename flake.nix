{
  description = "Trueflow dev environment";
  nixConfig = {
    warn-dirty = false;
  };
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
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

        # Common dependencies for git2 / openssl
        commonBuildInputs = with pkgs; [
          openssl
          pkg-config
        ] ++ lib.optionals stdenv.isDarwin [
          apple-sdk
          libiconv
        ];

        trueflow = rustPlatform.buildRustPackage {
          pname = "trueflow";
          version = "0.1.0";
          src = ./trueflow;
          cargoLock = { lockFile = ./trueflow/Cargo.lock; };
          
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = commonBuildInputs;
        };
      in {
        packages.default = trueflow;
        apps.default = flake-utils.lib.mkApp { drv = trueflow; };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            just
            
            # Cargo checks / lints / tools
            cargo-audit
            cargo-deny
            cargo-edit
            cargo-license
            cargo-pgo
            cargo-udeps
            cargo-watch
          ] ++ commonBuildInputs;

          shellHook = ''
            # Tells rust-analyzer where the stdlib sources are
            export RUST_SRC_PATH=${rustToolchain}/lib/rustlib/src/rust/library
            if [ -z "''${LC_COLLATE:-}" ]; then
              export LC_COLLATE=C
            fi
          '';
        };
      });
}
