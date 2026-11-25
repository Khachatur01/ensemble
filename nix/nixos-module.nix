# The NixOS module: a hub, `ensemble-hub serve`, and this computer's runner,
# `ensemble-runner serve`, each a user service of the person it is for, the
# hub's lingering so it runs from boot without a login. Each runs as the
# person it is for, with their `XDG_RUNTIME_DIR`, where `ensemble-hub …` and
# `ensemble-runner …` find its socket, from their `~/.local/state/ensemble/`
# (`hub/` and `runner/`, each its own), so they manage it with `ensemble-hub`
# and `ensemble-runner` as on any platform.
#
# The hub is only run from here, as a GitLab instance is: its settings and the
# keys of its backends are kept in its database, and changed with
# `ensemble-hub …`, its REST API or a web UI. It makes the settings a new hub
# starts with the first time it starts. Only where it listens, and a key a
# secrets manager keeps, through `environmentFile`, come from here.
#
# The runner is only run from here, too: its settings, the keys of its
# providers and its harnesses are made and changed with `ensemble-runner
# setup`, `… settings …`, `… provider …`, `… model …` and `… harness …`. It makes a
# runner named for the computer the first time it starts.
#
# The web UI, `ensemble-ui`, is optional and apart from the hub: a system
# service with no state of its own, on a port of its own, that serves the app
# and passes its calls on to a hub's REST API, this computer's or another's.
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
  runner = import ./runner.nix { inherit self lib; };

  hub = cfg.hub;

  runnerCfg = cfg.runner;

  ui = cfg.ui;
in
{
  options.services.ensemble = {
    hub = {
      enable = mkEnableOption ''
        an Ensemble hub, `ensemble-hub serve`: knowledge, tasks, accounts, and
        the link runners connect to. It runs as `services.ensemble.hub.user`,
        from their `~/.local/state/ensemble/hub/`, and they manage it with
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
          `~/.local/state/ensemble/hub/`: `core/`, its database, with its
          settings and the keys of its backends, sealed by `master.key` beside
          it, and a folder for each connector, such as `knowledge/vault/`.
          Back up the whole folder. For a hub of an account of its own, make
          that user, and run `ensemble-hub …` as it.
        '';
        example = "ana";
      };

      environmentFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          A file of NAME=value lines put in the hub's environment, such as a
          secret sops-nix or agenix puts in /run: the keys a secrets manager
          keeps, which a backend names with
          `ensemble-hub provider key <name> --key-env <NAME>`. A path as a
          string, so the keys never enter the Nix store. A key given to
          `ensemble-hub provider key` is kept in the hub's database instead.
        '';
      };

      listenAddress = mkOption {
        type = types.str;
        default = "127.0.0.1";
        description = ''
          The address the hub listens on, `serve --http-host`: 127.0.0.1 for
          this computer alone, 0.0.0.0 for other computers too.
        '';
        example = "0.0.0.0";
      };

      port = mkOption {
        type = types.port;
        default = 7700;
        description = "The port of the hub's REST API, MCP and runners' link, `serve --http-port`.";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Open `port` in the firewall. Without a TLS proxy in front, only on a
          network you trust.
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
    // runner.options { inherit pkgs; };

    ui = {
      enable = mkEnableOption ''
        the web UI of an Ensemble hub, `ensemble-ui`: the app on a port of its
        own, its calls passed on to the hub's REST API. People sign in with
        their name and password on the hub
      '';

      package = mkOption {
        type = types.package;
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.ensemble-ui;
        defaultText = lib.literalExpression "ensemble.packages.\${system}.ensemble-ui";
        description = "The package of `ensemble-ui`.";
      };

      hub = mkOption {
        type = types.str;
        default = "http://127.0.0.1:${toString hub.port}";
        defaultText = lib.literalExpression ''"http://127.0.0.1:''${toString config.services.ensemble.hub.port}"'';
        description = ''
          The hub's plain HTTP address, as this computer reaches it, `--hub`:
          this computer's hub, unless another is named.
        '';
        example = "http://10.0.0.5:7700";
      };

      listenAddress = mkOption {
        type = types.str;
        default = "127.0.0.1";
        description = ''
          The address the UI is served on, `--http-host`: 127.0.0.1 for this
          computer alone, 0.0.0.0 for other computers too.
        '';
        example = "0.0.0.0";
      };

      port = mkOption {
        type = types.port;
        default = 7701;
        description = "The port the UI is served on, `--http-port`.";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Open `port` in the firewall. Without a TLS proxy in front, only on a
          network you trust: passwords are typed into it.
        '';
      };
    };
  };

  config = mkMerge [
    {
      environment.systemPackages =
        lib.optional hub.enable hub.package ++ lib.optional runnerCfg.enable runnerCfg.package;
    }

    (mkIf ui.enable {
      networking.firewall.allowedTCPPorts = lib.optional ui.openFirewall ui.port;

      # It keeps nothing: the hub keeps the sessions, and the browser their
      # cookie.
      systemd.services.ensemble-ui = {
        description = "Ensemble web UI";
        wantedBy = [ "multi-user.target" ];
        after = [ "network.target" ];
        serviceConfig = {
          ExecStart = lib.escapeShellArgs [
            (lib.getExe ui.package)
            "--hub"
            ui.hub
            "--http-host"
            ui.listenAddress
            "--http-port"
            (toString ui.port)
          ];
          Restart = "on-failure";
          DynamicUser = true;
          NoNewPrivileges = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
        };
      };
    })

    (mkIf hub.enable {
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
        # The first time it starts, it makes the settings a new hub starts
        # with, and its vault; `ensemble-hub setup` adds its first admin.
        serviceConfig = {
          ExecStart = lib.escapeShellArgs [
            (lib.getExe hub.package)
            "serve"
            "--http-host"
            hub.listenAddress
            "--http-port"
            (toString hub.port)
          ];
          Restart = "on-failure";
          NoNewPrivileges = true;
        }
        // lib.optionalAttrs (hub.environmentFile != null) {
          EnvironmentFile = hub.environmentFile;
        };
      };
    })

    (mkIf runnerCfg.enable {
      systemd.user.services.ensemble-runner = {
        description = "Ensemble runner";
        wantedBy = [ "default.target" ];
        unitConfig.ConditionUser = runnerCfg.user;
        path = [
          runnerCfg.package
          pkgs.git
        ]
        ++ runnerCfg.extraPackages;
        # A runner named for this computer, the first time it starts.
        serviceConfig = {
          ExecStart = lib.escapeShellArgs (runner.command runnerCfg);
          Restart = "on-failure";
        }
        // lib.optionalAttrs (runnerCfg.environmentFile != null) {
          EnvironmentFile = runnerCfg.environmentFile;
        };
      };
    })
  ];
}
