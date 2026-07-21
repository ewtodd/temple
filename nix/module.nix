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
    auth_token_file = "${cfg.dataDir}/tokens";
    signal = {
      enabled = cfg.signal.enable;
      socket_addr = cfg.signal.socketAddr;
      # default_recipient and allowed_senders come from env vars
      # (SIGNAL_RECIPIENT) via environmentFile, not config.toml — phone
      # numbers are secrets and shouldn't be in the nix store.
      default_recipient = "";
      allowed_senders = [ ];
    };
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
      researcher_model = cfg.researcherModel;
    };
    ssh_targets = map (t: {
      name = t.name;
      account = t.account;
      host = t.host;
      port = t.port;
      owner = t.owner;
      allowed_dirs = t.allowedDirs;
    }) cfg.sshTargets;
    ssh_bastion = cfg.sshBastion;
    ssh_key_path = cfg.sshKeyPath;
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

    researcherModel = mkOption {
      type = types.str;
      default = "gemma-4-31b";
      description = "Model for research/lookup queries (Signal quick questions).";
    };

    sshTargets = mkOption {
      type = types.listOf (types.submodule {
        options = {
          name = mkOption { type = types.str; description = "Human-readable name (e.g. e-work@e-desktop)."; };
          account = mkOption { type = types.str; description = "SSH username on the workstation."; };
          host = mkOption { type = types.str; description = "Workstation IP or hostname."; };
          port = mkOption { type = types.port; default = 2222; description = "SSH port."; };
          owner = mkOption { type = types.str; description = "Temple username who owns this account."; };
          allowedDirs = mkOption { type = types.listOf types.str; default = [ ]; description = "Extra allowed dirs (beyond $HOME and /tmp)."; };
        };
      });
      default = [ ];
      description = "SSH targets for remote tool execution.";
    };

    sshBastion = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "deploy@10.0.0.2:2222";
      description = "Bastion host for ProxyJump SSH connections.";
    };

    sshKeyPath = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = "/run/agenix/temple-ssh-key";
      description = "SSH private key for connecting to workstations.";
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
      type = types.nullOr (types.either types.path (types.listOf types.path));
      default = null;
      example = "/run/agenix/temple-env";
      description = ''
        EnvironmentFile for secrets (systemd-style). Can be a single path
        or a list of paths — all are loaded. Any of these env vars are
        picked up:
        - LITELLM_API_KEY or LITELLM_MASTER_KEY (litellm auth)
        - SIGNAL_RECIPIENT (your phone number, E.164 with + prefix;
          used as outbound recipient AND sole allowed sender for inbound)
        Compatible with agenix secrets (one KEY=VALUE per line).
      '';
    };

    allowedDirs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "/etc/nixos" "/home" ];
      description = "Extra directories the agent may access without prompting (beyond each session's CWD).";
    };

    signal = {
      enable = mkEnableOption "Signal bot (two-way notifications via signal-cli)";

      socketAddr = mkOption {
        type = types.str;
        default = "127.0.0.1:7583";
        description = "signal-cli daemon TCP socket address.";
      };

      # Phone numbers (recipient + allowed senders) are secrets — they come
      # from the signalEnvironmentFile as SIGNAL_RECIPIENT (your number).
      # The recipient is also added as the sole allowed sender.
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
    environment.systemPackages = [ cfg.package ];
    environment.etc."temple/config.toml".source = configFile;

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
        EnvironmentFile = if builtins.isList cfg.environmentFile then cfg.environmentFile else [ cfg.environmentFile ];
      });
    };

    networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall [
      (lib.toInt port)
    ];
  };
}
