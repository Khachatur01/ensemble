# What the NixOS and Home Manager modules share about a runner: its options.
# They only run it. Its settings, the keys of its providers and its hub tokens
# are kept in its database, `~/.local/state/ensemble/runner/core/database.db`,
# and changed with `ensemble-runner setup`, `… settings …`, `… provider …`,
# `… model …`, `… harness …` and `… login`. The first time it starts, `ensemble-runner serve` makes a runner named
# for the computer, working in `~/ensemble`.
{ self, lib }:
let
  inherit (lib) mkOption types;
in
{
  options =
    { pkgs }:
    {
      package = mkOption {
        type = types.package;
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.ensemble-runner;
        defaultText = lib.literalExpression "ensemble.packages.\${system}.ensemble-runner";
        description = "The package of `ensemble-runner`.";
      };

      environmentFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          A file of NAME=value lines put in the runner's environment, such as
          a secret sops-nix or agenix puts in /run: the keys a secrets manager
          keeps, which a backend names with
          `ensemble-runner provider key <name> --key-env <NAME>`. A path as a
          string, so the keys never enter the Nix store. A key given to
          `ensemble-runner provider key` is kept in the runner's database
          instead.
        '';
      };

      extraPackages = mkOption {
        type = types.listOf types.package;
        default = [ ];
        description = ''
          Programs on the runner's PATH: the agent CLIs its harnesses run, such
          as Claude Code or OpenCode. git is always there.
        '';
        example = lib.literalExpression "[ pkgs.claude-code pkgs.opencode ]";
      };
    };

  # `ensemble-runner serve`.
  command = cfg: [
    (lib.getExe cfg.package)
    "serve"
  ];

  # `ensemble-runner serve` for a shell, with the environment file's
  # variables when there is one: for launchd, which reads no such file.
  shellCommand =
    cfg:
    lib.optionalString (cfg.environmentFile != null) "set -a; . ${lib.escapeShellArg cfg.environmentFile}; set +a; "
    + "exec ${lib.escapeShellArgs [ (lib.getExe cfg.package) "serve" ]}";
}
