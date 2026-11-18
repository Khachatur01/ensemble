# The Home Manager module: this computer's runner, `ensemble-runner serve`,
# for the person whose home it is, on Linux (a systemd user service) and on
# macOS (a launchd agent). What its settings and teams should always be may be
# written in Nix, like the NixOS module's runner; the rest is made with
# `ensemble-runner`, as on any platform. It runs once there is a runner.
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
  yaml = pkgs.formats.yaml { };
  runner = import ./runner.nix { inherit self lib; };
  files = runner.files { inherit pkgs yaml cfg; };
  links = runner.linkScript {
    inherit pkgs files;
    inherit (cfg) package;
  };
  path = lib.makeBinPath ([ cfg.package pkgs.git ] ++ cfg.extraPackages);
  # Written by `ensemble-runner setup`, with what `settings` declares merged in.
  runnerYaml = "${config.xdg.configHome}/ensemble/runner/runner.yaml";
  command = [
    (lib.getExe cfg.package)
    "serve"
  ]
  ++ lib.optionals (cfg.environmentFile != null) [
    "--env-file"
    cfg.environmentFile
  ];
in
{
  options.services.ensemble.runner = {
    enable = mkEnableOption ''
      this computer's Ensemble runner, `ensemble-runner serve`: it works on the
      tasks of its hub in the git repositories of its workspace
    '';
  }
  // runner.options { inherit pkgs yaml; };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    # The runner file and the teams, linked in on every switch.
    home.activation.ensembleRunner = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
      run ${links}
    '';

    systemd.user.services.ensemble-runner = mkIf pkgs.stdenv.hostPlatform.isLinux {
      Unit = {
        Description = "Ensemble runner";
        ConditionPathExists = runnerYaml;
      };
      Service = {
        ExecStart = lib.escapeShellArgs command;
        Environment = [ "PATH=${path}" ];
        Restart = "on-failure";
      };
      Install.WantedBy = [ "default.target" ];
    };

    launchd.agents.ensemble-runner = mkIf pkgs.stdenv.hostPlatform.isDarwin {
      enable = true;
      config = {
        ProgramArguments = command;
        EnvironmentVariables.PATH = "${path}:/usr/bin:/bin";
        # Kept running while there is a runner to run.
        KeepAlive.PathState.${runnerYaml} = true;
        RunAtLoad = true;
        StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/ensemble-runner.log";
      };
    };
  };
}
