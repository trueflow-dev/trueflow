{ pkgs ? import <nixpkgs> { } }:
let
  defaults = {
    retries = 3;
    labels = {
      tier = "backend";
      oncall = "platform";
    };

    # keep monitored packages visible to review
    packages = [
      pkgs.git
      { name = "helper"; enabled = true; }
    ];
  };

  mkWorker = name:
    let
      # build package set from the environment
      packageSet = with pkgs; [
        git
        ripgrep
      ];
    in {
      inherit name packageSet;
      meta = assert name != ""; {
        role = "worker";
      };
    };

  selected = if pkgs.stdenv.isLinux then { system = "linux"; } else { system = "other"; };
in {
  inherit defaults selected;
  worker = mkWorker "api";
}
