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

  configFile = toml.generate "temple-config.toml" ({
    listen = cfg.listen;
    litellm_url = cfg.litellmUrl;
    db_path = "${cfg.dataDir}/memory.db";
    allowed_dirs = cfg.allowedDirs;
    signal = {
      enabled = cfg.signal.enable;
      socket_addr = cfg.signal.socketAddr;
      default_recipient = "";
      allowed_senders = [ ];
    };
    nextcloud = {
      enabled = cfg.nextcloud.enable;
      server_url = cfg.nextcloud.serverUrl;
      username = cfg.nextcloud.username;
    };
    models = {
      default_model = cfg.defaultModel;
      simple_model = cfg.simpleModel;
      planner_model = cfg.plannerModel;
      executor_model = cfg.executorModel;
      reviewer_model = cfg.reviewerModel;
      critical_model = cfg.criticalModel;
      researcher_model = cfg.researcherModel;
      router_model = if cfg.routerModel != null then cfg.routerModel else cfg.researcherModel;
    };
  });

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
      description = "Default model for new sessions and Medium complexity.";
    };

    simpleModel = mkOption {
      type = types.str;
      default = "qwen3-4b-instruct";
      description = "Model for Simple queries.";
    };

    plannerModel = mkOption {
      type = types.str;
      default = "deepseek-v4-flash-high";
      description = "Planner model for Complex pipeline.";
    };

    executorModel = mkOption {
      type = types.str;
      default = "qwen3.6-27b-coding";
      description = "Executor model for Complex pipeline.";
    };

    reviewerModel = mkOption {
      type = types.str;
      default = "deepseek-v4-flash-high";
      description = "Reviewer model for Complex pipeline.";
    };

    criticalModel = mkOption {
      type = types.str;
      default = "deepseek-v4-flash-high";
      description = "Model for Critical complexity queries.";
    };

    researcherModel = mkOption {
      type = types.str;
      default = "gemma-4-31b";
      description = "Model for research/lookup queries.";
    };

    routerModel = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "gemma-4-12b-router";
      description = ''
        Model for complexity classification. Falls back to researcherModel
        when unset. Point this at a small fast model co-resident with the
        executor model on the same GPU host.
      '';
    };

    defaultPermission = mkOption {
      type = types.enum [ "default" "ask" "lockdown" "yolo" ];
      default = "default";
      description = "Default permission mode for new sessions.";
    };

    dataDir = mkOption {
      type = types.path;
      default = "/var/lib/temple";
      description = "State directory: memory DB, tokens, authorized keys.";
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
        - SIGNAL_RECIPIENT (E.164 phone number, + prefix)
      '';
    };

    allowedDirs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "/etc/nixos" "/home" ];
      description = "Extra directories the agent may access without prompting.";
    };

    signal = {
      enable = mkEnableOption "Signal bot (two-way notifications via signal-cli)";

      socketAddr = mkOption {
        type = types.str;
        default = "127.0.0.1:7583";
        description = "signal-cli daemon TCP socket address.";
      };
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
      description = "Open the WebSocket port in the firewall.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Extra CLI arguments for temple-server.";
    };

    gitSafeDirectories = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "/etc/nixos" ];
      description = ''
        Git repositories the temple user may open despite not owning them.
      '';
    };

    # ── Daemon authentication ──
    daemonAuthorizedKeys = mkOption {
      type = types.attrsOf types.str;
      default = { };
      example = {
        ethan = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI... e-work@e-desktop";
        val = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI... v-work@v-desktop";
      };
      description = ''
        Public keys authorized for daemon authentication, keyed by username.
        Each entry becomes a file at /var/lib/temple/authorized_keys/<name>.
        Reuses the user's existing SSH public key — no separate key needed.
      '';
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    environment.etc."temple/config.toml".source = configFile;

    users.users.temple = {
      isSystemUser = true;
      group = "temple";
      description = "temple renco agent";
      home = cfg.dataDir;
      shell = pkgs.bash;
    };
    users.groups.temple = { };

    # Authorized daemon keys
    systemd.tmpfiles.rules =
      [
        "d ${cfg.dataDir} 0750 temple temple - -"
        "d ${cfg.dataDir}/authorized_keys 0700 temple temple - -"
      ]
      ++ mapAttrsToList (
        username: key:
        "f ${cfg.dataDir}/authorized_keys/${username} 0400 temple temple - ${key}\n"
      ) cfg.daemonAuthorizedKeys
      ++ lib.optional (cfg.gitSafeDirectories != [ ])
        "L+ ${cfg.dataDir}/.gitconfig - - - - /etc/temple/gitconfig";

    # gitconfig marking configured repos as safe for the temple user
    environment.etc."temple/gitconfig" = mkIf (cfg.gitSafeDirectories != [ ]) {
      text = ''
        [safe]
        ${concatStringsSep "\n" (map (d: "\tdirectory = ${d}") cfg.gitSafeDirectories)}
      '';
    };

    systemd.services.temple-server = {
      description = "temple agent harness server — renco";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment.RUST_LOG = "temple_server=info";
      environment.HOME = cfg.dataDir;

      path = [ pkgs.nix ];

      serviceConfig = {
        Type = "simple";
        User = "temple";
        Group = "temple";
        ExecStart = concatStringsSep " " (
          [
            (escapeShellArgs [
              "${cfg.package}/bin/temple-server"
              "--config"
              (toString configFile)
            ])
          ]
          ++ map escapeShellArg cfg.extraArgs
        );
        Restart = "always";
        RestartSec = "5s";

        StateDirectory = "temple";
        StateDirectoryMode = "0750";
        UMask = "0077";

        NoNewPrivileges = true;
        ProtectSystem = "full";
        ProtectHome = "read-only";
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictSUIDSGID = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        LockPersonality = true;
        RemoveIPC = true;
        CapabilityBoundingSet = "";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
      }
      // (optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile =
          if builtins.isList cfg.environmentFile then cfg.environmentFile else [ cfg.environmentFile ];
      });
    };

    networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall [
      (lib.toIntBase10 port)
      8080
    ];
  };
}
