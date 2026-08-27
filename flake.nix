{
  description = "shen-rust — Shen language port to Rust with AWS Cedar integration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        tools = [
          pkgs.rustc
          pkgs.cargo
          pkgs.rustfmt
          pkgs.clippy
          pkgs.rust-analyzer
          pkgs.pkg-config
        ];
      in {
        packages.toolchain = pkgs.buildEnv {
          name = "shen-rust-toolchain";
          paths = tools;
        };
        packages.default = self.packages.${system}.toolchain;

        devShells.default = pkgs.mkShell {
          packages = tools;

          shellHook = ''
            echo "shen-rust dev shell"
            echo "  rustc: $(rustc --version)"
            echo "  cargo: $(cargo --version)"
          '';
        };
      });
}
