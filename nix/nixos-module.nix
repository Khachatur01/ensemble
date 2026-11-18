# The NixOS module: Ensemble installed, and its processes kept running, as on
# any other platform. It writes no configuration: `node.yaml`, `hub.yaml`, the
# teams, `.env` and the accounts are in `~/.config/ensemble/`, made and changed
# with `ensemble` itself (`ensemble node setup`, `ensemble hub init`,
# `ensemble node team …`, `ensemble hub user …`).
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    mkEnableOption
    mkIf
    mkMerge
    mkOption
    types
    ;
  cfg = config.services.ensemble;
  package = cfg.package;
in
{
  options.services.ensemble = {
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "ensemble.packages.\${system}.default";
      description = "The Ensemble package.";
    };

    node = {
      enable = mkEnableOption ''
        this computer's Ensemble node, `ensemble node serve`, as a user service
        of `services.ensemble.node.user`. It starts once that user has a node,
        `~/.config/ensemble/node/node.yaml`, which `ensemble node setup` writes
      '';

      user = mkOption {
        type = types.str;
        description = "The person the node works for: its user service runs for them alone.";
        example = "ana";
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

    hub = {
      enable = mkEnableOption ''
        an Ensemble hub, `ensemble hub serve`, as a system service that runs as
        `services.ensemble.hub.user` and serves their
        `~/.config/ensemble/hub/`. It starts once there is a `hub.yaml` there,
        which `ensemble hub init` writes
      '';

      user = mkOption {
        type = types.str;
        description = ''
          Whose hub it is: it runs as them, from their `~/.config/ensemble/hub/`,
          so they manage it with `ensemble hub …` as themselves.
        '';
        example = "ana";
      };
    };
  };

  config = mkMerge [
    (mkIf (cfg.node.enable || cfg.hub.enable) {
      environment.systemPackages = [ package ];
    })

    (mkIf cfg.node.enable {
      systemd.user.services.ensemble-node = {
        description = "Ensemble node";
        wantedBy = [ "default.target" ];
        unitConfig = {
          ConditionUser = cfg.node.user;
          ConditionPathExists = "%h/.config/ensemble/node/node.yaml";
        };
        path = [
          package
          pkgs.git
        ]
        ++ cfg.node.extraPackages;
        serviceConfig = {
          ExecStart = "${lib.getExe package} node serve";
          Restart = "on-failure";
        };
      };
    })

    (mkIf cfg.hub.enable {
      systemd.services.ensemble-hub = {
        description = "Ensemble hub";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        unitConfig.ConditionPathExists = "${
          config.users.users.${cfg.hub.user}.home
        }/.config/ensemble/hub/hub.yaml";
        # The vault is a git repository.
        path = [ pkgs.git ];
        serviceConfig = {
          ExecStart = "${lib.getExe package} hub serve";
          User = cfg.hub.user;
          Restart = "on-failure";
          NoNewPrivileges = true;
          PrivateTmp = true;
        };
      };
    })
  ];
}
