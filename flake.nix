{
  description = "ensemble — an agentic development system: a hub of services and nodes of computing power";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      # The systems nixpkgs builds the toolchain for.
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        ensemble = pkgs.rustPlatform.buildRustPackage {
          pname = "ensemble";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # The integration suite spawns the built binary, shell scripts that
          # stand in for the agent CLIs, and loopback sockets. All of that works
          # in the sandbox; none of it needs the network or a model server.
          doCheck = true;
          # Every crate's tests, not only the root package's.
          cargoTestFlags = [ "--workspace" ];
          # The worktree tests make real git repositories and worktrees.
          nativeCheckInputs = [ pkgs.git ];

          meta = {
            description = "An agentic development system: a hub of services and nodes of computing power, reached over MCP";
            mainProgram = "ensemble";
          };
        };
        default = ensemble;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      apps = forAll (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.ensemble}/bin/ensemble";
        };
      });

      # A hub as a system service and this computer's node as a user service,
      # each run as the person it is for: `services.ensemble.{hub,node}`.
      # Their files are written in Nix, or made with `ensemble`.
      nixosModules.default = import ./nix/nixos-module.nix { inherit self; };
      # This computer's node for one person, on Linux or macOS:
      # `services.ensemble.node`.
      homeManagerModules.default = import ./nix/home-manager-module.nix { inherit self; };

      checks = forAll (
        pkgs:
        {
          inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) ensemble;
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # The NixOS module evaluates, and makes the hub's and the node's units.
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
              "ensemble-hub.service" = system.config.systemd.units."ensemble-hub.service".unit;
              "ensemble-node.service" = system.config.systemd.user.units."ensemble-node.service".unit;
            };
        }
      );
    };
}
