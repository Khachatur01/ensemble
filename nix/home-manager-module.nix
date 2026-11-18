# The Home Manager module: this computer's node, `ensemble node serve`, kept
# running for the person whose home it is, on Linux (a systemd user service)
# and on macOS (a launchd agent). It writes no configuration: the node is set
# up with `ensemble node setup`, as on any other platform, and runs once it is.
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
  path = lib.makeBinPath ([ cfg.package pkgs.git ] ++ cfg.extraPackages);
  nodeYaml = "${config.xdg.configHome}/ensemble/node/node.yaml";
  command = [
    (lib.getExe cfg.package)
    "node"
    "serve"
  ];
in
{
  options.services.ensemble.node = {
    enable = mkEnableOption ''
      this computer's Ensemble node, `ensemble node serve`. It starts once
      there is a node, `~/.config/ensemble/node/node.yaml`, which
      `ensemble node setup` writes
    '';
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "ensemble.packages.\${system}.default";
      description = "The Ensemble package.";
    };
    extraPackages = mkOption {
      type = types.listOf types.package;
      default = [ ];
      description = ''
        Programs on the node's PATH: the agent CLIs its teams run, such as
        Claude Code or OpenCode. git is always there.
      '';
      example = lib.literalExpression "[ pkgs.claude-code pkgs.opencode ]";
    };
  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    systemd.user.services.ensemble-node = mkIf pkgs.stdenv.hostPlatform.isLinux {
      Unit = {
        Description = "Ensemble node";
        ConditionPathExists = nodeYaml;
      };
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
        # Kept running while there is a node to run.
        KeepAlive.PathState.${nodeYaml} = true;
        RunAtLoad = true;
        StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/ensemble-node.log";
      };
    };
  };
}
