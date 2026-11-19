# What the NixOS and Home Manager modules share about a runner: its options,
# and the files they become. The settings declared in Nix are merged into the
# runner's own `runner.yaml`, each time, setting by setting, so the rest of it
# stays the person's to change with `ensemble-runner config`, which refuses to
# change one Nix declares. Each team declared in Nix is a folder
# with a `config.yaml`, written from Nix or taken from a path (such as a flake
# input that pins a company's team repository), linked into the runner's teams
# directory, `~/.config/ensemble/runner/teams/`, and read-only there. Nothing
# here adds a setting: Ensemble refuses a file with one missing, and says
# which.
{ self, lib }:
let
  inherit (lib) mkOption types;
in
{
  options =
    { pkgs, yaml }:
    {
      package = mkOption {
        type = types.package;
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.ensemble-runner;
        defaultText = lib.literalExpression "ensemble.packages.\${system}.ensemble-runner";
        description = "The package of `ensemble-runner`.";
      };

      settings = mkOption {
        type = types.nullOr yaml.type;
        default = null;
        description = ''
          Settings of the runner's `runner.yaml`, as Nix: any of `version`,
          `name`, `works_for`, `workspace`, `hub` and `llm`, down to one
          setting such as `llm.direct.shares`. They are merged into
          `~/.config/ensemble/runner/runner.yaml` whenever the runner starts;
          the settings not declared here are made and changed with
          `ensemble-runner` (`ensemble-runner setup`, `ensemble-runner config
          set`, `ensemble-runner login`), which refuses to change those declared
          here. `ensemble-runner init --stdout` prints every setting. Null
          leaves the whole file to `ensemble-runner`.
        '';
        example = lib.literalExpression ''
          {
            version = 1;
            name = "laptop";
            works_for = "owner";
            hub = "wss://hub.example.com";
            llm.direct = {
              backends = { };
              shares = [ ];
              max_shared_calls = 1;
            };
          }
        '';
      };

      workspace = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          The folder the runner works in, its `workspace` in `runner.yaml`:
          each git repository directly inside it is a project whose tasks the
          runner runs, and one added later is found within half a minute.
          `~/` is the person's home directory. Merged into `runner.yaml` like
          `settings`, in place of a `workspace` there. Null leaves it to
          `settings`, or to `ensemble-runner setup`.
        '';
        example = "~/ensemble";
      };

      teams = mkOption {
        type = types.attrsOf (
          types.submodule {
            options = {
              settings = mkOption {
                type = types.nullOr yaml.type;
                default = null;
                description = ''
                  The team's `config.yaml`, as Nix. `ensemble-runner team new --stdout`
                  prints every setting.
                '';
              };
              source = mkOption {
                type = types.nullOr types.path;
                default = null;
                description = ''
                  A folder with the team's `config.yaml`, such as a flake input
                  of the team's repository. Used instead of `settings`.
                '';
              };
            };
          }
        );
        default = { };
        description = ''
          The runner's helper teams, by name; their agents are `<name>/<agent>`.
          Each is linked into `~/.config/ensemble/runner/teams/`, beside the
          teams made there by hand (`ensemble-runner team new`, `pull`, `link`),
          which are left alone. A team declared here is read-only to
          `ensemble-runner`.
        '';
        example = lib.literalExpression ''
          {
            web.source = inputs.acme-web-team;
            docs.settings = {
              version = 1;
              limits = { };
              gates = { };
              agents = { };
            };
          }
        '';
      };

      environmentFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          A file of NAME=value lines, the API keys the runner's LLM backends name
          in `api_key_env`, given to `ensemble-runner serve` as `--env-file`, such
          as a secret sops-nix or agenix puts in /run. A path as a string, so
          the keys never enter the Nix store. Null reads the runner's own
          `~/.config/ensemble/runner/.env`, when there is one.
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

  # The runner file and the team folders, in the store.
  files =
    { pkgs, yaml, cfg }:
    let
      declared =
        (if cfg.settings == null then { } else cfg.settings)
        // lib.optionalAttrs (cfg.workspace != null) { inherit (cfg) workspace; };
    in
    {
      runnerYaml =
        if cfg.settings == null && cfg.workspace == null then
          null
        else
          yaml.generate "runner.yaml" declared;
      teams = lib.mapAttrs (
        name: team:
        if team.source != null then
          team.source
        else if team.settings != null then
          pkgs.runCommand "ensemble-team-${name}" { } ''
            mkdir $out
            cp ${yaml.generate "config.yaml" team.settings} $out/config.yaml
          ''
        else
          throw "services.ensemble.runner.teams.${name}: give it `settings` or `source`"
      ) cfg.teams;
    };

  # A script that merges the settings declared in Nix into the runner file,
  # links the teams into the runner's folder, and takes away links to teams
  # this configuration no longer declares.
  linkScript =
    {
      pkgs,
      files,
      package,
    }:
    let
      teams = lib.concatStringsSep " " (lib.attrNames files.teams);
    in
    pkgs.writeShellScript "ensemble-runner-links" ''
      set -eu
      dir="''${XDG_CONFIG_HOME:-$HOME/.config}/ensemble/runner"
      mkdir -p "$dir/teams"
      chmod 700 "$dir"
      ${
        if files.runnerYaml != null then
          ''
            # A runner to merge into: as `ensemble-runner setup` makes one.
            if [ ! -e "$dir/runner.yaml" ]; then
              printf 'version: 1\nname: %s\nworks_for: owner\nworkspace: ~/ensemble\nllm:\n  direct:\n    backends: {}\n    shares: []\n    max_shared_calls: 1\ninterfaces:\n  cli: {}\n' \
                "$(${pkgs.hostname}/bin/hostname -s)" > "$dir/runner.yaml"
            fi
            ${lib.getExe package} config merge ${files.runnerYaml}
          ''
        else
          ''rm -f "$dir/.nix-managed"''
      }
      ${lib.concatStringsSep "\n" (
        lib.mapAttrsToList (name: path: ''ln -sfn ${path} "$dir/teams/${name}"'') files.teams
      )}
      for link in "$dir"/teams/*; do
        [ -L "$link" ] || continue
        case "$(readlink "$link")" in
          /nix/store/*) ;;
          *) continue ;;
        esac
        name="$(basename "$link")"
        case " ${teams} " in
          *" $name "*) ;;
          *) rm "$link" ;;
        esac
      done
    '';
}
