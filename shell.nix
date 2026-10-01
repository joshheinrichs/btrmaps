{
  pkgs ? import (import ./sources.nix).nixpkgs-src { },
}:
let
  btrmaps = import ./. { inherit pkgs; };
in
pkgs.mkShell {
  inputsFrom = [ btrmaps ];
  packages = [
    pkgs.clippy
    pkgs.rustfmt
    pkgs.rust-analyzer
  ];
  # `cargo run` builds a binary without the rpath default.nix patches in.
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath btrmaps.runtimeLibs;
}
