# The NixOS module: a hub, `ensemble-hub serve`, as a system service, and this
# computer's runner, `ensemble-runner serve`, as a user service. Each runs as the
# person it is for, from their `~/.config/ensemble/`, so they manage it with
# `ensemble-hub` and `ensemble-runner` as on any platform. What should always
# hold is written in Nix: the settings declared here are merged into
# `hub.yaml` and `runner.yaml` when each starts, and teams are linked in. The
# rest is made and changed with `ensemble-hub init`, `ensemble-runner setup`,
# `… config set` and `ensemble-runner team …`, which refuse to change what Nix
# declares. Each process starts once its file is there.
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
  runner = import ./runner.nix { inherit self lib; };

  hub = cfg.hub;
  hubDir = "${config.users.users.${hub.user}.home}/.config/ensemble/hub";
  hubYaml = if hub.settings == null then null else yaml.generate "hub.yaml" hub.settings;

  runnerCfg = cfg.runner;
  runnerFiles = runner.files {
    inherit pkgs yaml;
    cfg = runnerCfg;
  };
in
{
  options.services.ensemble = {
    hub = {
      enable = mkEnableOption ''
        an Ensemble hub, `ensemble-hub serve`: knowledge, tasks, accounts, and
        the link runners connect to. It runs as `services.ensemble.hub.user`,
        from their `~/.config/ensemble/hub/`, and they manage it with
        `ensemble-hub …` as themselves
      '';

      package = mkOption {
        type = types.package;
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.ensemble-hub;
        defaultText = lib.literalExpression "ensemble.packages.\${system}.ensemble-hub";
        description = "The package of `ensemble-hub`.";
      };

      user = mkOption {
        type = types.str;
        description = ''
          Whose hub it is: it runs as them, and its files are in their
          `~/.config/ensemble/hub/`. For a hub of an account of its own, make
          that user, and run `ensemble-hub …` as it.
        '';
        example = "ana";
      };

      settings = mkOption {
        type = types.nullOr yaml.type;
        default = null;
        description = ''
          Settings of the hub's `hub.yaml`, as Nix: any of `version`, `owner`,
          `http`, `llm`, `accounts` and `connectors`, down to one setting
          such as `http.listen`. They are merged into the hub's
          `hub.yaml` whenever it starts; the rest is made and changed with
          `ensemble-hub init` and `ensemble-hub config set`, which refuse to
          change those declared here. `ensemble-hub init --stdout` prints every
          setting. Null leaves the whole file to `ensemble-hub`, and the hub
          starts once `ensemble-hub init` has written it.
        '';
        example = lib.literalExpression ''
          {
            version = 1;
            owner = "acme";
            llm.direct.backends.central = {
              type = "openai-compatible-api";
              base_url = "http://127.0.0.1:8000/v1";
              timeout_seconds = 300;
            };
            http.listen = "127.0.0.1:7700";
            accounts = {
              registration = { type = "approval"; max_pending = 20; };
              sessions.hours = 12;
              lockout = { failures = 5; minutes = 15; };
              roles = {
                admin = [ "admin" "share" "recall" "tasks" "compute" "host" ];
                member = [ "share" "recall" "tasks" "compute" ];
              };
            };
            connectors = {
              rest = { };
              mcp = { };
              runners = { };
              knowledge = {
                vault = "vault";
                distill = {
                  try = [ { backend = "central"; model = "qwen3.5-72b"; } ];
                  when_none = "wait";
                  max_tokens = 4000;
                };
                recall = { max_notes = 8; max_chars = 6000; };
              };
              tasks.trackers = { };
              sources.adapters = { };
            };
          }
        '';
      };

      environmentFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          A file of NAME=value lines, the API keys the hub's backends name in
          `api_key_env`, given to `ensemble-hub serve` as `--env-file`, such
          as a secret sops-nix or agenix puts in /run. A path as a string, so
          the keys never enter the Nix store. Null reads the hub's own
          `~/.config/ensemble/hub/.env`, when there is one.
        '';
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Open the port `settings.http.listen` names. Without a TLS proxy in
          front, only on a network you trust. Needs `settings.http.listen`.
        '';
      };
    };

    runner = {
      enable = mkEnableOption ''
        this computer's Ensemble runner, `ensemble-runner serve`, as a user
        service of `services.ensemble.runner.user`: it works on the tasks of
        its hub in the git repositories of its workspace
      '';

      user = mkOption {
        type = types.str;
        description = "The person the runner works for: its user service runs for them alone.";
        example = "ana";
      };
    }
    // runner.options { inherit pkgs yaml; };
  };

  config = mkMerge [
    {
      environment.systemPackages =
        lib.optional hub.enable hub.package ++ lib.optional runnerCfg.enable runnerCfg.package;
    }

    (mkIf hub.enable {
      assertions = [
        {
          assertion = hub.openFirewall -> (hub.settings.http.listen or null) != null;
          message = "services.ensemble.hub.openFirewall needs services.ensemble.hub.settings.http.listen, which names the port";
        }
      ];

      networking.firewall.allowedTCPPorts = lib.optional hub.openFirewall (
        lib.toInt (lib.last (lib.splitString ":" hub.settings.http.listen))
      );

      systemd.services.ensemble-hub = {
        description = "Ensemble hub";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        restartTriggers = lib.optional (hubYaml != null) hubYaml;
        # Without settings, it waits for `ensemble-hub init`.
        unitConfig = lib.optionalAttrs (hubYaml == null) {
          ConditionPathExists = "${hubDir}/hub.yaml";
        };
        # The vault is a git repository.
        path = [ pkgs.git ];
        preStart =
          if hubYaml != null then
            ''
              # A hub to merge into: the sample, with its vault.
              [ -e ${hubDir}/hub.yaml ] || ${lib.getExe hub.package} init
              ${lib.getExe hub.package} config merge ${hubYaml}
            ''
          else
            ''
              rm -f ${hubDir}/.nix-managed
            '';
        serviceConfig = {
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe hub.package)
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

    (mkIf runnerCfg.enable {
      systemd.user.services.ensemble-runner = {
        description = "Ensemble runner";
        wantedBy = [ "default.target" ];
        unitConfig = {
          ConditionUser = runnerCfg.user;
        }
        # Without settings, it waits for `ensemble-runner setup`.
        // lib.optionalAttrs (runnerFiles.runnerYaml == null) {
          ConditionPathExists = "%h/.config/ensemble/runner/runner.yaml";
        };
        restartTriggers =
          lib.optional (runnerFiles.runnerYaml != null) runnerFiles.runnerYaml
          ++ lib.attrValues runnerFiles.teams;
        path = [
          runnerCfg.package
          pkgs.git
        ]
        ++ runnerCfg.extraPackages;
        serviceConfig = {
          ExecStartPre = runner.linkScript {
            inherit pkgs;
            inherit (runnerCfg) package;
            files = runnerFiles;
          };
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe runnerCfg.package)
              "serve"
            ]
            ++ lib.optionals (runnerCfg.environmentFile != null) [
              "--env-file"
              runnerCfg.environmentFile
            ]
          );
          Restart = "on-failure";
        };
      };
    })
  ];
}
