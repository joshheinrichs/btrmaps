{
  # git ls-remote https://github.com/NixOS/nixpkgs nixos-unstable
  nixpkgs-src = builtins.fetchGit {
    url = "https://github.com/NixOS/nixpkgs";
    rev = "e94cb152ed51bd6e24eb4a41f1460252beb52cd2";
    ref = "nixos-unstable";
    shallow = true;
  };
}
