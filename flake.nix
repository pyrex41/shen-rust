{
  description = "shen-rust — Nix-managed Rust development environment";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      each = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      tools = pkgs: [ pkgs.rustc pkgs.cargo pkgs.rustfmt pkgs.clippy pkgs.rust-analyzer pkgs.pkg-config pkgs.git ];
    in {
      packages = each (pkgs: { toolchain = pkgs.buildEnv { name = "shen-rust-toolchain"; paths = tools pkgs; }; default = pkgs.buildEnv { name = "shen-rust-toolchain"; paths = tools pkgs; }; });
      devShells = each (pkgs: { default = pkgs.mkShell { packages = tools pkgs; }; });
    };
}
