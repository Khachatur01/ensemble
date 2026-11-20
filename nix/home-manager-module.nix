# The Home Manager module: this computer's runner, `ensemble-runner serve`,
# for the person whose home it is, on Linux (a systemd user service) and on
# macOS (a launchd agent). It only runs the runner: its settings, keys and
# teams are made and changed with `ensemble-runner`, as on any platform.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib) mkEnableOption mkIf;
  cfg = config.services.ensemble.runner;
  runner = import ./runner.nix { inherit self lib; };
  path = lib.makeBinPath ([ cfg.package pkgs.git ] ++ cfg.extraPackages);
  command = runner.command cfg;
in
{
  options.services.ensemble.runner = {
    enable = mkEnableOption ''
      this computer's Ensemble runner, `ensemble-runner serve`: it works on the
      tasks of its hub in the git repositories of its workspace
    '';
  }
  // runner.options { inherit pkgs; };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    systemd.user.services.ensemble-runner = mkIf pkgs.stdenv.hostPlatform.isLinux {
      Unit.Description = "Ensemble runner";
      Service = {
        ExecStart = lib.escapeShellArgs command;
        Environment = [ "PATH=${path}" ];
        Restart = "on-failure";
      }
      // lib.optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = cfg.environmentFile;
      };
      Install.WantedBy = [ "default.target" ];
    };

    launchd.agents.ensemble-runner = mkIf pkgs.stdenv.hostPlatform.isDarwin {
      enable = true;
      config = {
        ProgramArguments = [
          "/bin/sh"
          "-c"
          (runner.shellCommand cfg)
        ];
        EnvironmentVariables.PATH = "${path}:/usr/bin:/bin";
        KeepAlive = true;
        RunAtLoad = true;
        StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/ensemble-runner.log";
      };
    };
  };
}
