{
  description = "smtp_to_telegram: a small SMTP server that forwards every incoming email to Telegram";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        smtp-to-telegram = pkgs.callPackage ./nix/package.nix { };
        default = smtp-to-telegram;
      });

      nixosModules.default = import ./nix/module.nix self;

      checks = forAllSystems (
        pkgs:
        let
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in
        {
          # Builds and runs the unit and integration tests.
          inherit package;

          clippy = package.overrideAttrs (old: {
            pname = "smtp-to-telegram-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --offline --all-targets -- -D warnings
              runHook postBuild
            '';
            doCheck = false;
            installPhase = "touch $out";
          });

          fmt =
            pkgs.runCommand "smtp-to-telegram-fmt"
              {
                nativeBuildInputs = [ pkgs.rustfmt ];
              }
              ''
                cd ${package.src}
                rustfmt --check --edition 2024 src/main.rs src/lib.rs tests/*.rs
                touch $out
              '';

          nixos = import ./nix/test.nix {
            inherit pkgs self;
          };
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = [
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.swaks
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
