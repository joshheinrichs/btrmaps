{
  pkgs ? import (import ./sources.nix).nixpkgs-src { },
}:
let
  btrmaps = pkgs.rustPlatform.buildRustPackage {
    pname = "btrmaps";
    version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
    src = pkgs.lib.fileset.toSource {
      root = ./.;
      fileset = pkgs.lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./src
      ];
    };
    cargoLock.lockFile = ./Cargo.lock;
    # sudo is left to PATH: its setuid binary lives outside the store, in a
    # different place per distro (/run/wrappers/bin on NixOS).
    XDG_OPEN = "${pkgs.xdg-utils}/bin/xdg-open";
    # egui dlopens the Wayland, X11 and GL libraries at runtime.
    postFixup = ''
      patchelf --add-rpath ${
        pkgs.lib.makeLibraryPath [
          pkgs.wayland
          pkgs.libxkbcommon
          pkgs.libGL
          pkgs.libx11
          pkgs.libxcursor
          pkgs.libxrandr
          pkgs.libxi
        ]
      } $out/bin/btrmaps
    '';
    passthru.tests.e2e = import ./test.nix { inherit pkgs btrmaps; };
    meta = {
      description = "Map of btrfs space along a Hilbert curve";
      license = with pkgs.lib.licenses; [
        mit
        asl20
      ];
      platforms = pkgs.lib.platforms.linux;
      mainProgram = "btrmaps";
    };
  };
in
btrmaps
