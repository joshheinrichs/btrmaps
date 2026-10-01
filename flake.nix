{
  description = "Map of btrfs space along a Hilbert curve";

  # The same nixpkgs as sources.nix, which nix-build and nix-shell use; bump both.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/e94cb152ed51bd6e24eb4a41f1460252beb52cd2";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      each = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = each (pkgs: {
        default = import ./. { inherit pkgs; };
      });
      devShells = each (pkgs: {
        default = import ./shell.nix { inherit pkgs; };
      });
      checks = each (pkgs: {
        e2e = (import ./. { inherit pkgs; }).tests.e2e;
      });
    };
}
