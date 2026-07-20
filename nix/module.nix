# temple-server NixOS module.
# Imported via the temple flake:
#   imports = [ temple.nixosModules.temple-server ];
{
  config,
  lib,
  pkgs,
  templePackage,
  ...
}:
with lib;
let
  cfg = config.services.temple-server;
  toml = pkgs.formats.toml { };
  configFile = toml.generate "temple-config.toml" {
    listen = cfg.listen;
    litellm_url = cfg.litellmUrl;
    db_path = "${cfg.dataDir}/memory.db";
    allowed_dirs = cfg.allowedDirs;
    default_permission = cfg.defaultPermission;
    nextcloud = {
      enabled = cfg.nextcloud.enable;
      server_url = cfg.nextcloud.serverUrl;
      username = cfg.nextcloud.username;
    };
    models = {
      use_local_router = false;
      default_model = cfg.defaultModel;
      simple_model = cfg.simpleModel;
      planner_model = cfg.plannerModel;
      executor_model = cfg.executorModel;
      reviewer_model = cfg.reviewerModel;
      critical_model = cfg.criticalModel;
    };
    local_llama_url = cfg.localLlamaUrl;
    local_llama_model = cfg.localLlamaModel;
  };
  port = toString (lib.last (lib.splitString ":" cfg.listen));
in
{
  options.services.temple-server = {
    enable = mkEnableOption "temple agent harness server (renco)";

    package = mkOption {
      type = types.package;
      default = templePackage;
      description = "The temple package providing temple-server.";
    };

    listen = mkOption {
      type = types.str;
      default = "0.0.0.0:42123";
      description = "WebSocket listen address (host:port).";
    };

    litellmUrl = mkOption {
      type = types.str;
      default = "https://llm.ethanwtodd.com";
      description = "LiteLLM proxy URL used for model access and MCP tools.";
    };

    defaultModel = mkOption {
      type = types.str;
      default = "deepseek-v4-flash-high";
      description = "Default model for new sessions and Medium complexity (must exist on the litellm proxy).";
    };

    simpleModel = mkOption {
      type = types.str;
      default = "qwen3-4b-instruct";
      description = "Model for Simple queries (local classifier on oracle).";
    };

    plannerModel = mkOption {
      type = types.str;
      default = "deepseek-v4-flash-high";
      description = "Planner model for Complex pipeline (deepseek on son-of-anton).";
    };

    executorModel = mkOption {
      type = types.str;
      default = "qwen3.6-27b-coding";
      description = "Executor model for Complex pipeline (qwen coding on anton).";
    };

    reviewerModel = mkOption {
      type = types.str;
      default = "deepseek-v4-flash-high";
      description = "Reviewer model for Complex pipeline (deepseek on son-of-anton).";
    };

    criticalModel = mkOption {
      type = types.str;
      default = "deepseek-v4-flash-high";
      description = "Model for Critical complexity queries (deepseek direct).";
    };

    defaultPermission = mkOption {
      type = types.enum [ "default" "ask" "lockdown" "yolo" ];
      default = "default";
      description = "Default permission mode for new sessions.";
    };

    dataDir = mkOption {
      type = types.path;
      default = "/var/lib/temple";
      description = "State directory: memory DB, skills.md, etc.";
    };

    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = "/run/agenix/temple-env";
      description = ''
        EnvironmentFile for secrets. Must define LITELLM_API_KEY.
        Compatible with agenix secrets (a file containing
        LITELLM_API_KEY=sk-...). Other secrets (NTFY_TOKEN,
        NEXTCLOUD_PASSWORD) are picked up from the environment too.
      '';
    };

    allowedDirs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "/etc/nixos" "/home" ];
      description = "Extra directories the agent may access without prompting (beyond each session's CWD).";
    };

    nextcloud = {
      enable = mkEnableOption "Nextcloud calendar integration";
      serverUrl = mkOption {
        type = types.str;
        default = "https://cloud.ethanwtodd.com";
      };
      username = mkOption {
        type = types.str;
        default = "renco";
      };
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = "Open the WebSocket port in the firewall (needed for LAN clients).";
    };

    localLlamaUrl = mkOption {
      type = types.str;
      default = "http://127.0.0.1:8080/v1";
      description = "Local llama.cpp endpoint for routing/title generation (on oracle, zero-latency).";
    };

    localLlamaModel = mkOption {
      type = types.str;
      default = "qwen3-4b-instruct";
      description = "Small model name for local routing and title generation.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Extra CLI arguments for temple-server.";
    };
  };

  config = mkIf cfg.enable {
    users.users.temple = {
      isSystemUser = true;
      group = "temple";
      description = "temple renco agent";
    };
    users.groups.temple = { };

    systemd.services.temple-server = {
      description = "temple agent harness server — renco";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment.RUST_LOG = "temple_server=info";

      serviceConfig = {
        Type = "simple";
        User = "temple";
        Group = "temple";
        ExecStart = concatStringsSep " " (
          [
            "${cfg.package}/bin/temple-server"
            "--config ${configFile}"
          ]
          ++ cfg.extraArgs
        );
        Restart = "always";
        RestartSec = "5s";

        StateDirectory = "temple";
        StateDirectoryMode = "0750";

        NoNewPrivileges = true;
        ProtectSystem = "full";
        ProtectHome = "read-only";
        PrivateTmp = true;
        ProtectKernelTunables = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
      }
      // (optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = cfg.environmentFile;
      });
    };

    networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall [
      (lib.toInt port)
    ];
  };
}
