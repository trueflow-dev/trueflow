{ pkgs ? import <nixpkgs> { } }:
let
  maxRetries = 3;

  config = {
    name = "sample";
    threshold = 4;
  };

  mkMultiplier = factor: {
    inherit factor;
    process = values: builtins.map (value: value * factor) values;
  };

  collectUntil = limit:
    let
      go = current:
        if current >= limit then [ ] else [ current ] ++ go (current + 1);
    in go 0;

  multiplier = mkMultiplier 2;
  values = collectUntil config.threshold;
  processed = multiplier.process values;

  attempts =
    builtins.genList (index: "attempt ${builtins.toString index}") maxRetries;
in { inherit config values processed attempts; }
