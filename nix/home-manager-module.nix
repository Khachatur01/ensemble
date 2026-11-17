# The Home Manager module: this computer's node, `ensemble node serve`, for
# the person whose home it is, on Linux (a systemd user service) and on macOS
# (a launchd agent), configured in Nix like the NixOS module's node.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkEnableOption mkIf mkOption types;
  cfg = config.services.ensemble.node;
  yaml = pkgs.formats.yaml { };
  node = import ./node.nix { inherit lib; };
  files = node.files { inherit pkgs yaml cfg; };
  links = node.linkScript { inherit pkgs files; };
  path = lib.makeBinPath ([ cfg.package pkgs.git ] ++ cfg.extraPackages);
  command = [
    (lib.getExe cfg.package)
    "node"
    "serve"
  ]
  ++ lib.optionals (cfg.environmentFile != null) [
    "--env-file"
    cfg.environmentFile
  ];
in
{
  options.services.ensemble.node = {
    enable = mkEnableOption ''
      this computer's Ensemble node, `ensemble node serve`: your agent
      sessions reach it through `ensemble mcp`
    '';
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "ensemble.packages.\${system}.default";
      description = "The Ensemble package.";
    };
  }
  // node.options { inherit pkgs yaml; };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    # The node file and the teams, linked in on every switch.
    home.activation.ensembleNode = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
      run ${links}
    '';

    systemd.user.services.ensemble-node = mkIf pkgs.stdenv.hostPlatform.isLinux {
      Unit.Description = "Ensemble node";
      Service = {
        ExecStart = lib.escapeShellArgs command;
        Environment = [ "PATH=${path}" ];
        Restart = "on-failure";
      };
      Install.WantedBy = [ "default.target" ];
    };

    launchd.agents.ensemble-node = mkIf pkgs.stdenv.hostPlatform.isDarwin {
      enable = true;
      config = {
        ProgramArguments = command;
        EnvironmentVariables.PATH = "${path}:/usr/bin:/bin";
        KeepAlive = true;
        RunAtLoad = true;
        StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/ensemble-node.log";
      };
    };
  };
}
