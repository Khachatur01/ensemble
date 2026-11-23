{
  description = "ensemble — an agentic development system: a hub of services and runners of computing power";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      # The systems nixpkgs builds the toolchain for.
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      inherit (nixpkgs) lib;
    in
    {
      packages = forAll (
        pkgs:
        let
          # One executable of the workspace, tested with the crates named in
          # `tests`. The two packages test the whole workspace between them,
          # each crate once: the runner the crates it is built of, the hub
          # its own and the end-to-end suite, which runs both executables.
          executable =
            {
              name,
              description,
              tests,
              env ? { },
            }:
            pkgs.rustPlatform.buildRustPackage {
              pname = name;
              version = "0.1.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;
              cargoBuildFlags = [
                "--bin"
                name
              ];
              inherit env;

              # The suites spawn the built executables, shell scripts that stand
              # in for the agent CLIs, and loopback sockets. All of that works in
              # the sandbox; none of it needs the network or a model server.
              doCheck = true;
              cargoTestFlags = lib.concatMap (crate: [
                "-p"
                crate
              ]) tests;
              # The worktree tests and the vault make real git repositories.
              nativeCheckInputs = [ pkgs.git ];

              meta = {
                inherit description;
                mainProgram = name;
              };
            };
        in
        rec {
          # The hub's web UI, built for wasm32 as ui/build.sh builds it: the
          # files the hub embeds and serves at `/`.
          ensemble-ui = pkgs.stdenv.mkDerivation {
            pname = "ensemble-ui";
            version = "0.1.0";
            src = ./ui;
            cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./ui/Cargo.lock; };
            nativeBuildInputs = with pkgs; [
              cargo
              rustc
              lld
              wasm-bindgen-cli
              rustPlatform.cargoSetupHook
            ];
            buildPhase = ''
              runHook preBuild
              cargo build --release --offline --target wasm32-unknown-unknown
              wasm-bindgen --target web --no-typescript --out-dir dist --out-name ensemble_ui \
                target/wasm32-unknown-unknown/release/ensemble-ui.wasm
              cp static/* dist/
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              cp -r dist $out
              runHook postInstall
            '';
            meta.description = "The web UI of an Ensemble hub";
          };
          ensemble-runner = executable {
            name = "ensemble-runner";
            description = "This computer's Ensemble runner: it runs the tasks of a hub with your helper teams";
            tests = [
              "ensemble-runner"
              "ensemble-tasks"
              "ensemble-agents"
              "ensemble-link"
              "ensemble-backends"
              "ensemble-config"
              "ensemble-ops"
              "ensemble-core"
            ];
          };
          ensemble-hub = executable {
            name = "ensemble-hub";
            description = "An Ensemble hub: accounts, tasks, knowledge, MCP for client agents, and the link runners connect to";
            tests = [
              "ensemble-hub"
              "ensemble-knowledge"
              "ensemble-store"
              "ensemble"
            ];
            env.ENSEMBLE_UI_DIST = "${ensemble-ui}";
          };
          # Both executables.
          default = pkgs.symlinkJoin {
            name = "ensemble";
            paths = [
              ensemble-hub
              ensemble-runner
            ];
            meta = {
              description = "An agentic development system: a hub of services that client agents reach over MCP, and runners of computing power";
              mainProgram = "ensemble-runner";
            };
          };
        }
      );

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            # The web UI: wasm32 links with lld, and wasm-bindgen-cli makes its
            # JavaScript glue, of the version ui/Cargo.toml pins.
            lld
            wasm-bindgen-cli
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      apps = forAll (
        pkgs:
        let
          packages = self.packages.${pkgs.stdenv.hostPlatform.system};
          app = name: {
            type = "app";
            program = "${packages.${name}}/bin/${name}";
          };
        in
        {
          ensemble-hub = app "ensemble-hub";
          ensemble-runner = app "ensemble-runner";
          default = app "ensemble-runner";
        }
      );

      # A hub as a system service and this computer's runner as a user service,
      # each run as the person it is for: `services.ensemble.{hub,runner}`.
      # Their files are written in Nix, or made with `ensemble-hub` and
      # `ensemble-runner`.
      nixosModules.default = import ./nix/nixos-module.nix { inherit self; };
      # This computer's runner for one person, on Linux or macOS:
      # `services.ensemble.runner`.
      homeManagerModules.default = import ./nix/home-manager-module.nix { inherit self; };

      checks = forAll (
        pkgs:
        {
          inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) ensemble-hub ensemble-runner;
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # The NixOS module evaluates, and makes the hub's and the runner's units.
          nixos-module =
            let
              system = nixpkgs.lib.nixosSystem {
                inherit (pkgs.stdenv.hostPlatform) system;
                modules = [
                  self.nixosModules.default
                  ./nix/example.nix
                  {
                    boot.loader.grub.enable = false;
                    fileSystems."/" = {
                      device = "/dev/null";
                      fsType = "ext4";
                    };
                    system.stateVersion = "25.05";
                  }
                ];
              };
            in
            pkgs.linkFarm "ensemble-nixos-module" {
              "ensemble-hub.service" = system.config.systemd.user.units."ensemble-hub.service".unit;
              "ensemble-runner.service" = system.config.systemd.user.units."ensemble-runner.service".unit;
            };
        }
      );
    };
}
