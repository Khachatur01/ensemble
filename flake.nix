{
  description = "ensemble — one interface for several models, delegating work between them";

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

          # The integration suite spawns the built binary, binds loopback
          # sockets and shells out to stub scripts. All of that works in the
          # sandbox; none of it needs the network, a model server or a Claude
          # subscription.
          doCheck = true;

          meta = {
            description = "One interface for several models, delegating work between them";
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
          # `claude-code` is deliberately absent. It is unfree, so listing it
          # would make `nix develop` fail for anyone who has not set
          # allowUnfree — and it is needed at *run* time, only by the
          # claude-code-cli backend, never to build or to test. Install it
          # separately if you configure that backend.
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
