# An example NixOS configuration: one person's computer that runs a hub for
# themselves and a runner. Nix only runs them: the settings of each are kept
# in its database, and the person manages both with `ensemble-hub` and
# `ensemble-runner` as themselves.
# The flake's `checks` evaluate it, so it stays a configuration that works.
{ pkgs, ... }:
{
  services.ensemble.hub = {
    enable = true;
    user = "ana";
    environmentFile = "/run/secrets/ensemble-hub.env";
  };

  # Its web UI, at http://127.0.0.1:7701.
  services.ensemble.ui.enable = true;

  services.ensemble.runner = {
    enable = true;
    user = "ana";
    environmentFile = "/run/secrets/ensemble-runner.env";
    extraPackages = [ pkgs.hello ];
  };

  users.users.ana = {
    isNormalUser = true;
    home = "/home/ana";
  };
}
