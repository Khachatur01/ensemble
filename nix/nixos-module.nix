# The NixOS module: a hub, `ensemble hub serve`, as a system service of its
# own user, and this computer's node, `ensemble node serve`, as a user service
# of the person it works for. Both are configured in Nix: `hub.yaml`,
# `node.yaml` and the node's teams are written from it.
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
  # The hub's home: its `~/.config/ensemble/hub/` is under it.
  hubHome = "/var/lib/ensemble-hub";
  hubYaml = yaml.generate "hub.yaml" hub.settings;
  hubPort = lib.toInt (lib.last (lib.splitString ":" hub.settings.server.listen));
  hubEnv = {
    HOME = hubHome;
    XDG_CONFIG_HOME = hubHome;
    XDG_STATE_HOME = "${hubHome}/state";
    XDG_CACHE_HOME = "${hubHome}/cache";
  };
  # `ensemble hub …` as the hub's user, for the admin: `sudo ensemble-hub user add root --role admin`.
  hubAdmin = pkgs.writeShellScriptBin "ensemble-hub" ''
    exec ${pkgs.util-linux}/bin/runuser -u ${hub.user} -- \
      ${lib.concatStringsSep " " (lib.mapAttrsToList (k: v: "${k}=${v}") hubEnv)} \
      ${lib.getExe package} hub "$@"
  '';

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
        an Ensemble hub, `ensemble hub serve`: knowledge, the task inbox,
        accounts, and the link nodes connect to. It runs as its own user, with
        its files in ${hubHome}/ensemble/hub/; manage its accounts with
        `sudo ensemble-hub user …`
      '';

      settings = mkOption {
        type = yaml.type;
        description = ''
          The hub's `hub.yaml`, as Nix: `version`, `owner`, `llm_backends`,
          `knowledge`, `server`, `roles`, and optionally `tasks`.
          `ensemble hub init --stdout` prints every setting. Relative paths,
          such as `knowledge.vault`, are under ${hubHome}/ensemble/hub/.
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
          `api_key_env`. A path as a string, so the keys never enter the Nix
          store.
        '';
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Open the port `server.listen` names. Without a TLS proxy in front,
          only on a network you trust.
        '';
      };

      user = mkOption {
        type = types.str;
        default = "ensemble-hub";
        description = "The user the hub runs as.";
      };

      group = mkOption {
        type = types.str;
        default = "ensemble-hub";
        description = "The group the hub runs as.";
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
      users.users.${hub.user} = {
        isSystemUser = true;
        group = hub.group;
        home = hubHome;
        description = "Ensemble hub";
      };
      users.groups.${hub.group} = { };

      environment.systemPackages = [ hubAdmin ];
      networking.firewall.allowedTCPPorts = lib.optional hub.openFirewall hubPort;

      systemd.services.ensemble-hub = {
        description = "Ensemble hub";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        restartTriggers = [ hubYaml ];
        # The vault is a git repository.
        path = [ pkgs.git ];
        environment = hubEnv;
        preStart = ''
          install -d -m 700 ${hubHome}/ensemble/hub
          install -m 600 ${hubYaml} ${hubHome}/ensemble/hub/hub.yaml
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
          Group = hub.group;
          StateDirectory = "ensemble-hub";
          StateDirectoryMode = "0700";
          UMask = "0077";
          Restart = "on-failure";
          NoNewPrivileges = true;
          PrivateTmp = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          ProtectKernelTunables = true;
          ProtectControlGroups = true;
          RestrictSUIDSGID = true;
        };
      };
    })

    (mkIf nodeCfg.enable {
      systemd.user.services.ensemble-node = {
        description = "Ensemble node";
        wantedBy = [ "default.target" ];
        unitConfig.ConditionUser = nodeCfg.user;
        restartTriggers = lib.optional (nodeFiles.nodeYaml != null) nodeFiles.nodeYaml
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
