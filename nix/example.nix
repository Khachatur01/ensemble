# An example NixOS configuration: one person's computer that runs a hub for
# themselves and a runner that names it, both written in Nix, with one helper
# team. She manages them with `ensemble-hub` and `ensemble-runner` as herself.
# The flake's `checks` evaluate it, so it stays a configuration that works.
{ pkgs, ... }:
{
  services.ensemble.hub = {
    enable = true;
    user = "ana";
    environmentFile = "/run/secrets/ensemble-hub.env";
    settings = {
      version = 1;
      owner = "ana";
      llm_backends.local = {
        type = "openai-compatible-api";
        base_url = "http://127.0.0.1:11434/v1";
        timeout_seconds = 300;
      };
      knowledge = {
        vault = "vault";
        distill = {
          try = [
            {
              backend = "local";
              model = "qwen3.5:9b";
            }
          ];
          when_none = "wait";
          max_tokens = 4000;
        };
        recall = {
          max_notes = 8;
          max_chars = 6000;
        };
      };
      server = {
        listen = "127.0.0.1:7700";
        registration.type = "closed";
        sessions.hours = 12;
        lockout = {
          failures = 5;
          minutes = 15;
        };
      };
      roles.admin = [
        "admin"
        "share"
        "recall"
        "tasks"
        "compute"
        "host"
      ];
    };
  };

  services.ensemble.runner = {
    enable = true;
    user = "ana";
    environmentFile = "/run/secrets/ensemble-runner.env";
    workspace = "~/ensemble";
    extraPackages = [ pkgs.hello ];
    settings = {
      version = 1;
      name = "laptop";
      access = "private";
      hub = "ws://127.0.0.1:7700";
      offers = { };
    };
    teams.web.settings = {
      version = 1;
      llm_backends.local = {
        type = "openai-compatible-api";
        base_url = "http://127.0.0.1:11434/v1";
        timeout_seconds = 600;
      };
      limits = { };
      gates = { };
      agents.summariser = {
        prompt = "You summarise what you are given in five bullet points or fewer.";
        max_seconds = 600;
        max_concurrent = 1;
        isolation = "shared";
        budget.type = "unlimited";
        adapter = {
          type = "direct";
          try = [
            {
              backend = "local";
              model = "qwen3.5:9b";
            }
          ];
          when_none = "fail";
          max_tokens = 1000;
        };
      };
    };
  };

  users.users.ana = {
    isNormalUser = true;
    home = "/home/ana";
  };
}
