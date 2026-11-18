# An example NixOS configuration: one person's computer that runs a hub for
# themselves and a node. Both are set up with `ensemble` as on any platform
# (`ensemble hub init`, `ensemble node setup`); the module keeps them running.
# The flake's `checks` evaluate it, so it stays a configuration that works.
{ pkgs, ... }:
{
  services.ensemble.hub = {
    enable = true;
    user = "ana";
  };

  services.ensemble.node = {
    enable = true;
    user = "ana";
    extraPackages = [ pkgs.hello ];
  };

  users.users.ana = {
    isNormalUser = true;
    home = "/home/ana";
  };
}
