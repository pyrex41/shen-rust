{
  description = "shen-cedar — Shen language port to Rust with AWS Cedar integration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            pkgs.rustc
            pkgs.cargo
            pkgs.rustfmt
            pkgs.clippy
            pkgs.rust-analyzer
            pkgs.pkg-config
          ];

          shellHook = ''
            echo "shen-cedar dev shell"
            echo "  rustc: $(rustc --version)"
            echo "  cargo: $(cargo --version)"
          '';
        };
      });
}
