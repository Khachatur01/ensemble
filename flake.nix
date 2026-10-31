{
  description = "ensemble — a team of coding agents behind one MCP server";

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
          # The worktree tests make real git repositories and worktrees.
          nativeCheckInputs = [ pkgs.git ];

          meta = {
            description = "A team of coding agents (Claude Code, OpenCode) behind one MCP server";
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

      checks = forAll (pkgs: { inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) ensemble; });
    };
}
