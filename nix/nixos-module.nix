# The NixOS module: a hub, `ensemble-hub serve`, and this computer's runner,
# `ensemble-runner serve`, each a user service of the person it is for, the
# hub's lingering so it runs from boot without a login. Each runs as the
# person it is for, with their `XDG_RUNTIME_DIR`, where `ensemble-hub …` and
# `ensemble-runner …` find its socket, from their `~/.config/ensemble/`, so they manage it with
# `ensemble-hub` and `ensemble-runner` as on any platform.
#
# The hub is only run from here, as a GitLab instance is: its settings and the
# keys of its backends are kept in its database, and changed with
# `ensemble-hub …`, its REST API or a web UI. It writes the sample settings the
# first time it starts. Only a key a secrets manager keeps comes from here,
# through `environmentFile`.
#
# The runner's settings declared here are merged into `runner.yaml` when it
# starts, and teams are linked in. The rest is made and changed with
# `ensemble-runner setup`, `… config set` and `ensemble-runner team …`, which
# refuse to change what Nix declares. It starts once its file is there.
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
          `~/.config/ensemble/hub/`: its database, with its settings and the
          keys of its backends, sealed by `master.key` beside it. Back up
          both. For a hub of an account of its own, make that user, and run
          `ensemble-hub …` as it.
        '';
        example = "ana";
      };

      environmentFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          A file of NAME=value lines, given to `ensemble-hub serve` as
          `--env-file`, such as a secret sops-nix or agenix puts in /run: the
          keys a secrets manager keeps, which a backend names with
          `ensemble-hub backend key <name> --key-env <NAME>`. A path as a
          string, so the keys never enter the Nix store. A key given to
          `ensemble-hub backend key` is kept in the hub's database instead.
          Null reads the hub's own `~/.config/ensemble/hub/.env`, when there
          is one.
        '';
      };

      port = mkOption {
        type = types.nullOr types.port;
        default = null;
        description = ''
          The port of the hub's `http.listen`, one of its settings, for
          `openFirewall` to open. Set `http.listen` to an address on it with
          `ensemble-hub config set http.listen 0.0.0.0:<port>`.
        '';
        example = 7700;
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Open `port` in the firewall. Without a TLS proxy in front, only on a
          network you trust. Needs `port`.
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
          assertion = hub.openFirewall -> hub.port != null;
          message = "services.ensemble.hub.openFirewall needs services.ensemble.hub.port, the port of the hub's http.listen";
        }
      ];

      networking.firewall.allowedTCPPorts = lib.optional hub.openFirewall hub.port;

      # A user service, so it has its user's runtime directory, where
      # `ensemble-hub …` finds its socket; lingering, so it starts at boot.
      users.users.${hub.user}.linger = true;

      systemd.user.services.ensemble-hub = {
        description = "Ensemble hub";
        wantedBy = [ "default.target" ];
        unitConfig.ConditionUser = hub.user;
        # The vault is a git repository.
        path = [ pkgs.git ];
        # The sample settings, with its vault, the first time it starts; a hub
        # that has a hub.yaml still takes it in.
        preStart = ''
          ${lib.getExe hub.package} init --keep
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
          Restart = "on-failure";
          NoNewPrivileges = true;
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
