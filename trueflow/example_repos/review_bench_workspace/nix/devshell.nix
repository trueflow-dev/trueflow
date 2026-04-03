{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  packages = with pkgs; [
    bash
    cargo
    git
    just
    nodejs
    python3
    rustc
  ];

  shellHook = ''
    export REVIEW_BENCH_ENV=1
    echo "entered review bench dev shell"
  '';
}
