{
  description = "mare — hyperextensible AI coding agent harness (Rust core, Lua 5.5 plugins)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        version = cargoToml.workspace.package.version;
      in
      {
        packages = {
          mare = rustPlatform.buildRustPackage {
            pname = "mare";
            inherit version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "-p" "mare-cli" "--bin" "mare" ];
            doCheck = false;

            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake
              pkgs.perl
              pkgs.makeBinaryWrapper
            ];
            dontUseCmakeConfigure = true;
            dontCargoUseSystemLibs = true;

            postInstall = ''
              wrapProgram "$out/bin/mare" \
                --suffix PATH : ${pkgs.lib.makeBinPath [ pkgs.git ]}
              install -Dm644 LICENSE "$out/share/licenses/mare/LICENSE"
              install -Dm644 THIRD_PARTY_NOTICES.md "$out/share/licenses/mare/THIRD_PARTY_NOTICES.md"
              install -Dm644 crates/mare-kernel/lua/cordis/LICENSE "$out/share/licenses/mare/cordis-LICENSE"
            '';

            meta = with pkgs.lib; {
              description = "Hyperextensible AI coding agent harness (Rust core, Lua 5.5 plugins)";
              homepage = "https://github.com/signalridge/mare";
              license = licenses.asl20;
              maintainers = [ ];
              mainProgram = "mare";
            };
          };

          default = self.packages.${system}.mare;
        };

        apps = {
          mare = flake-utils.lib.mkApp {
            drv = self.packages.${system}.mare;
          };
          default = self.apps.${system}.mare;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.pkg-config
            pkgs.git
            pkgs.python3
          ];

          shellHook = ''
            echo "mare development shell"
            echo "Rust version: $(rustc --version)"
          '';
        };
      }
    )
    // {
      overlays.default = final: prev: {
        mare = self.packages.${prev.system}.mare;
      };
    };
}
