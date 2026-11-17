# What the NixOS and Home Manager modules share about a node: its options,
# and the files they become. A node's settings are `node.yaml` written as Nix;
# each team is a folder with a `config.yaml`, written from Nix or taken from a
# path (such as a flake input that pins a company's team repository), and
# linked into the node's teams directory, `~/.config/ensemble/node/teams/`.
# Nothing here adds a setting: Ensemble refuses a file with one missing, and
# says which.
{ lib }:
let
  inherit (lib) mkOption types;
in
{
  options =
    { pkgs, yaml }:
    {
      settings = mkOption {
        type = types.nullOr yaml.type;
        default = null;
        description = ''
          The node's `node.yaml`, as Nix: `version`, `name`, `access`, and
          optionally `hub` and `offers`. `ensemble node init --stdout` prints
          every setting. Null leaves `~/.config/ensemble/node/node.yaml` alone,
          for `ensemble setup` or your own hand.
        '';
        example = lib.literalExpression ''
          {
            version = 1;
            name = "laptop";
            access = "private";
            hub = "wss://hub.example.com";
            offers = { };
          }
        '';
      };

      teams = mkOption {
        type = types.attrsOf (
          types.submodule {
            options = {
              settings = mkOption {
                type = types.nullOr yaml.type;
                default = null;
                description = ''
                  The team's `config.yaml`, as Nix. `ensemble team new --stdout`
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
          The node's helper teams, by name; their agents are `<name>/<agent>`.
          Each is linked into `~/.config/ensemble/node/teams/`, beside the teams
          made there by hand (`ensemble team new`, `pull`, `add`), which are
          left alone. A team declared here is read-only to `ensemble`.
        '';
        example = lib.literalExpression ''
          {
            web.source = inputs.acme-web-team;
            docs.settings = {
              version = 1;
              llm_backends = { };
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
          A file of NAME=value lines, the API keys the teams' backends name in
          `api_key_env`, given to `ensemble node serve` as `--env-file`, such
          as a secret sops-nix or agenix puts in /run. A path as a string, so
          the keys never enter the Nix store. Null reads the node's own
          `~/.config/ensemble/node/.env`, when there is one.
        '';
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

  # The node file and the team folders, in the store.
  files =
    { pkgs, yaml, cfg }:
    {
      nodeYaml = if cfg.settings == null then null else yaml.generate "node.yaml" cfg.settings;
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
          throw "services.ensemble.node.teams.${name}: give it `settings` or `source`"
      ) cfg.teams;
    };

  # A script that links the node file and the teams into the node's folder,
  # and takes away links to teams this configuration no longer declares.
  linkScript =
    { pkgs, files }:
    let
      teams = lib.concatStringsSep " " (lib.attrNames files.teams);
    in
    pkgs.writeShellScript "ensemble-node-links" ''
      set -eu
      dir="''${XDG_CONFIG_HOME:-$HOME/.config}/ensemble/node"
      mkdir -p "$dir/teams"
      chmod 700 "$dir"
      ${lib.optionalString (files.nodeYaml != null) ''ln -sfn ${files.nodeYaml} "$dir/node.yaml"''}
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
