{
  description = "Postvim, a small single-buffer UTF-8 terminal editor";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          postvim = pkgs.rustPlatform.buildRustPackage {
            pname = manifest.package.name;
            version = manifest.package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
                ./docs
                ./AGENTS.md
                ./README.md
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;

            meta = {
              description = "Small single-buffer UTF-8 terminal editor";
              homepage = "https://github.com/marcybelardo/postvim";
              mainProgram = "postvim";
              platforms = systems;
            };
          };
        in
        {
          inherit postvim;
          default = postvim;
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          meta.description = "Postvim terminal editor";
          program = "${self.packages.${system}.postvim}/bin/postvim";
        };
      });

      checks = forAllSystems (system: {
        package = self.packages.${system}.postvim;
      });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
              python3
            ];
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        });
    };
}
