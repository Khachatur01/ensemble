# The NixOS module: a hub, `ensemble hub serve`, as a system service, and this
# computer's node, `ensemble node serve`, as a user service. Each runs as the
# person it is for, from their `~/.config/ensemble/`, so they manage it with
# `ensemble` as on any platform. `hub.yaml`, `node.yaml` and the node's teams
# may be written in Nix; left out, they are made with `ensemble` (`ensemble
# hub init`, `ensemble node setup`, `ensemble node team …`), and each process
# starts once its file is there.
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
  yaml = pkgs.formats.yaml { };
  node = import ./node.nix { inherit lib; };
  package = cfg.package;

  hub = cfg.hub;
  hubDir = "${config.users.users.${hub.user}.home}/.config/ensemble/hub";
  hubYaml = if hub.settings == null then null else yaml.generate "hub.yaml" hub.settings;

  nodeCfg = cfg.node;
  nodeFiles = node.files {
    inherit pkgs yaml;
    cfg = nodeCfg;
  };
in
{
  options.services.ensemble = {
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "ensemble.packages.\${system}.default";
      description = "The Ensemble package.";
    };

    hub = {
      enable = mkEnableOption ''
        an Ensemble hub, `ensemble hub serve`: knowledge, tasks, accounts, and
        the link nodes connect to. It runs as `services.ensemble.hub.user`,
        from their `~/.config/ensemble/hub/`, and they manage it with
        `ensemble hub …` as themselves
      '';

      user = mkOption {
        type = types.str;
        description = ''
          Whose hub it is: it runs as them, and its files are in their
          `~/.config/ensemble/hub/`. For a hub of an account of its own, make
          that user, and run `ensemble hub …` as it.
        '';
        example = "ana";
      };

      settings = mkOption {
        type = types.nullOr yaml.type;
        default = null;
        description = ''
          The hub's `hub.yaml`, as Nix: `version`, `owner`, `llm_backends`,
          `knowledge`, `server`, `roles`, and optionally `tasks`.
          `ensemble hub init --stdout` prints every setting. Written into the
          hub's folder on every start. Null leaves `hub.yaml` to
          `ensemble hub init` or your own hand, and the hub starts once it is
          there.
        '';
        example = lib.literalExpression ''
          {
            version = 1;
            owner = "acme";
            llm_backends.central = {
              type = "openai-compatible-api";
              base_url = "http://127.0.0.1:8000/v1";
              timeout_seconds = 300;
            };
            knowledge = {
              vault = "vault";
              distill = {
                try = [ { backend = "central"; model = "qwen3.5-72b"; } ];
                when_none = "wait";
                max_tokens = 4000;
              };
              recall = { max_notes = 8; max_chars = 6000; };
            };
            server = {
              listen = "127.0.0.1:7700";
              registration = { type = "approval"; max_pending = 20; };
              sessions.hours = 12;
              lockout = { failures = 5; minutes = 15; };
            };
            roles = {
              admin = [ "admin" "share" "recall" "tasks" "compute" "host" ];
              member = [ "share" "recall" "tasks" "compute" ];
            };
          }
        '';
      };

      environmentFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          A file of NAME=value lines, the API keys the hub's backends name in
          `api_key_env`, given to `ensemble hub serve` as `--env-file`, such
          as a secret sops-nix or agenix puts in /run. A path as a string, so
          the keys never enter the Nix store. Null reads the hub's own
          `~/.config/ensemble/hub/.env`, when there is one.
        '';
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Open the port `settings.server.listen` names. Without a TLS proxy in
          front, only on a network you trust. Needs `settings`.
        '';
      };
    };

    node = {
      enable = mkEnableOption ''
        this computer's Ensemble node, `ensemble node serve`, as a user service
        of `services.ensemble.node.user`: its agent sessions reach it through
        `ensemble mcp`
      '';

      user = mkOption {
        type = types.str;
        description = "The person the node works for: its user service runs for them alone.";
        example = "ana";
      };
    }
    // node.options { inherit pkgs yaml; };
  };

  config = mkMerge [
    (mkIf (hub.enable || nodeCfg.enable) {
      environment.systemPackages = [ package ];
    })

    (mkIf hub.enable {
      assertions = [
        {
          assertion = hub.openFirewall -> hub.settings != null;
          message = "services.ensemble.hub.openFirewall needs services.ensemble.hub.settings, whose server.listen names the port";
        }
      ];

      networking.firewall.allowedTCPPorts = lib.optional hub.openFirewall (
        lib.toInt (lib.last (lib.splitString ":" hub.settings.server.listen))
      );

      systemd.services.ensemble-hub = {
        description = "Ensemble hub";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        restartTriggers = lib.optional (hubYaml != null) hubYaml;
        # Without settings, it waits for `ensemble hub init`.
        unitConfig = lib.optionalAttrs (hubYaml == null) {
          ConditionPathExists = "${hubDir}/hub.yaml";
        };
        # The vault is a git repository.
        path = [ pkgs.git ];
        preStart = lib.optionalString (hubYaml != null) ''
          install -d -m 700 ${hubDir}
          install -m 600 ${hubYaml} ${hubDir}/hub.yaml
        '';
        serviceConfig = {
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe package)
              "hub"
              "serve"
            ]
            ++ lib.optionals (hub.environmentFile != null) [
              "--env-file"
              hub.environmentFile
            ]
          );
          User = hub.user;
          Restart = "on-failure";
          NoNewPrivileges = true;
          PrivateTmp = true;
        };
      };
    })

    (mkIf nodeCfg.enable {
      systemd.user.services.ensemble-node = {
        description = "Ensemble node";
        wantedBy = [ "default.target" ];
        unitConfig = {
          ConditionUser = nodeCfg.user;
        }
        # Without settings, it waits for `ensemble node setup`.
        // lib.optionalAttrs (nodeFiles.nodeYaml == null) {
          ConditionPathExists = "%h/.config/ensemble/node/node.yaml";
        };
        restartTriggers =
          lib.optional (nodeFiles.nodeYaml != null) nodeFiles.nodeYaml
          ++ lib.attrValues nodeFiles.teams;
        path = [
          package
          pkgs.git
        ]
        ++ nodeCfg.extraPackages;
        serviceConfig = {
          ExecStartPre = node.linkScript {
            inherit pkgs;
            files = nodeFiles;
          };
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe package)
              "node"
              "serve"
            ]
            ++ lib.optionals (nodeCfg.environmentFile != null) [
              "--env-file"
              nodeCfg.environmentFile
            ]
          );
          Restart = "on-failure";
        };
      };
    })
  ];
}
