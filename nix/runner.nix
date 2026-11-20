# What the NixOS and Home Manager modules share about a runner: its options.
# They only run it. Its settings and the keys of its backends are kept in its
# database, `~/.config/ensemble/runner/database.db`, and changed with
# `ensemble-runner setup`, `… config set`, `… backend …` and `… login`; its
# teams are added with `ensemble-runner team pull`, `link` or `new`. Before
# each start the service runs `ensemble-runner init --keep`, which makes a
# runner named for the computer, working in `~/ensemble`, when there is none.
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
          A file of NAME=value lines, given to `ensemble-runner serve` as
          `--env-file`, such as a secret sops-nix or agenix puts in /run: the
          keys a secrets manager keeps, which a backend names with
          `ensemble-runner backend key <name> --key-env <NAME>`. A path as a
          string, so the keys never enter the Nix store. A key given to
          `ensemble-runner backend key` is kept in the runner's database
          instead. Null reads the runner's own `~/.config/ensemble/runner/.env`,
          when there is one.
        '';
      };

      extraPackages = mkOption {
        type = types.listOf types.package;
        default = [ ];
        description = ''
          Programs on the runner's PATH: the agent CLIs its teams run, such as
          Claude Code or OpenCode. git is always there.
        '';
        example = lib.literalExpression "[ pkgs.claude-code pkgs.opencode ]";
      };
    };

  # `ensemble-runner serve`, with the environment file when there is one.
  command =
    cfg:
    [
      (lib.getExe cfg.package)
      "serve"
    ]
    ++ lib.optionals (cfg.environmentFile != null) [
      "--env-file"
      cfg.environmentFile
    ];

  # A runner to run, when it has none yet.
  init = cfg: "${lib.getExe cfg.package} init --keep";
}
