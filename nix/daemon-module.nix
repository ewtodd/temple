# temple-daemon NixOS module — one full agent service.
# Imported via the temple flake:
#   imports = [ temple.nixosModules.temple-daemon ];
#
# Creates a single systemd SYSTEM service (boot-starting, no login) running
# the complete agent: loop, session log, local tools (confined to the
# session cwd), sessions, memory bridge, cron, and the WebSocket front on
# 127.0.0.1. Session isolation is enforced per authenticated TUI client
# (pubkey → owner) and the Signal presence is built in — one DB, one
# process, both surfaces. The service runs under its own account so it is
# not tied to any person's desktop user.
{
  config,
  lib,
  pkgs,
  templePackage,
  ...
}:
with lib;
let
  cfg = config.services.temple-daemon;

  modelLines = [
    "default_model = \"${cfg.defaultModel}\""
    "simple_model = \"${cfg.simpleModel}\""
    "planner_model = \"${cfg.plannerModel}\""
    "executor_model = \"${cfg.executorModel}\""
    "reviewer_model = \"${cfg.reviewerModel}\""
    "researcher_model = \"${cfg.researcherModel}\""
    "router_model = \"${if cfg.routerModel != null then cfg.routerModel else cfg.researcherModel}\""
    "title_model = \"${if cfg.titleModel != null then cfg.titleModel else cfg.researcherModel}\""
  ];

  backendLines = mapAttrsToList (name: url: ''  "${name}" = "${url}"'') cfg.modelEndpoints;

  configText = concatStringsSep "\n" (
    [
      "listen = \"${cfg.listen}\""
      "listen_health = \"${cfg.listenHealth}\""
      "db_path = \"${cfg.stateDir}/temple.db\""
      "authorized_keys_dir = \"/etc/temple/keys\""
      "searxng_url = \"${cfg.searxngUrl}\""
      "default_permission = \"${cfg.defaultPermission}\""
      "allowed_dirs = ["
      (concatMapStringsSep ",\n" (d: "  \"${d}\"") cfg.allowedDirs)
      "]"
    ]
    ++ optional (cfg.authTokenFile != null) "auth_token_file = \"${toString cfg.authTokenFile}\""
    ++ optional cfg.signal.enable (
      concatStringsSep "\n" [
        ""
        "[signal]"
        "enabled = true"
        "socket_addr = \"${cfg.signal.socketAddr}\""
        "default_recipient = \"${cfg.signal.defaultRecipient}\""
        "allowed_senders = ["
        (concatMapStringsSep ",\n" (s: "  \"${s}\"") cfg.signal.allowedSenders)
        "]"
      ]
    )
    ++ optional cfg.openWebUI.enable (
      concatStringsSep "\n" [
        ""
        "[openwebui]"
        "enabled = true"
        "base_url = \"${cfg.openWebUI.baseUrl}\""
        "api_key_env = \"${cfg.openWebUI.apiKeyEnv}\""
      ]
    )
    ++ optional (cfg.modelEndpoints != { }) (
      concatStringsSep "\n" ([ "backends = {" ] ++ backendLines ++ [ "}" ])
    )
    ++ optional cfg.sandbox.enable (
      concatStringsSep "\n" (
        [ "" "[sandbox]" "enabled = true" ]
        ++ optional (cfg.sandbox.extraWritableDirs != [ ]) (
          concatStringsSep "\n" (
            [ "extra_writable_dirs = [" ]
            ++ map (d: "  \"${d}\"") cfg.sandbox.extraWritableDirs
            ++ [ "]" ]
          )
        )
      )
    )
    ++ [
      ""
      "[models]"
    ]
    ++ modelLines
  );
in
{
  options.services.temple-daemon = {
    enable = mkEnableOption "temple full-agent daemon";

    package = mkOption {
      type = types.package;
      default = templePackage;
    };

    serviceUser = mkOption {
      type = types.str;
      default = "temple";
      description = "System account the daemon runs as (created by the module).";
    };

    stateDir = mkOption {
      type = types.str;
      default = "/var/lib/temple";
      description = "State: DB, session logs, tokens.";
    };

    listen = mkOption {
      type = types.str;
      default = "127.0.0.1:42123";
      description = "WebSocket listen address (TUI + Signal clients).";
    };

    listenHealth = mkOption {
      type = types.str;
      default = "127.0.0.1:42223";
      description = "Health endpoint listen address.";
    };

    modelEndpoints = mkOption {
      type = types.attrsOf types.str;
      default = { };
      description = "Model name to llama.cpp/llama-swap endpoint mappings (with /v1).";
    };

    defaultModel = mkOption {
      type = types.str;
      default = "qwen3.6-35b-a3b";
    };
    simpleModel = mkOption {
      type = types.str;
      default = "qwen3.6-27b";
    };
    plannerModel = mkOption {
      type = types.str;
      default = "qwen3.6-35b-a3b";
    };
    executorModel = mkOption {
      type = types.str;
      default = "qwen3.6-27b";
    };
    reviewerModel = mkOption {
      type = types.str;
      default = "qwen3.6-35b-a3b";
    };
    researcherModel = mkOption {
      type = types.str;
      default = "qwen3.6-27b";
    };
    routerModel = mkOption {
      type = types.nullOr types.str;
      default = null;
    };
    titleModel = mkOption {
      type = types.nullOr types.str;
      default = null;
    };

    searxngUrl = mkOption {
      type = types.str;
      default = "http://127.0.0.1:8888/search";
      description = "SearXNG JSON API endpoint (must be reachable from this host).";
    };

    allowedDirs = mkOption {
      type = types.listOf types.str;
      default = [ "/etc/nixos" "/home" ];
    };

    defaultPermission = mkOption {
      type = types.str;
      default = "default";
    };

    sandbox = {
      enable = mkEnableOption "Landlock confinement for executed commands (read-only fs except allowed dirs / HOME / /tmp / /dev)";
      extraWritableDirs = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "/scratch" ];
        description = "Extra writable directories for sandboxed commands.";
      };
    };

    authTokenFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Auth token file for Signal /verify registration.";
    };

    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "EnvironmentFile for secrets (e.g. OPENWEBUI_API_KEY).";
    };

    supplementaryGroups = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "nixconfig" ];
      description = "Extra groups for the service account (e.g. the fleet's nix config group).";
    };

    readWritePaths = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "/etc/nixos" ];
      description = "Additional writable paths under ProtectSystem=full (e.g. the nix config repo).";
    };

    gitSafeDirectories = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "/etc/nixos" ];
      description = "Git repos the service may open despite not owning them (cron flake updates).";
    };

    signal = {
      enable = mkEnableOption "Signal presence (shared number)";
      socketAddr = mkOption {
        type = types.str;
        default = "127.0.0.1:7583";
        description = "signal-cli JSON-RPC socket (e.g. on mu).";
      };
      defaultRecipient = mkOption {
        type = types.str;
        default = "";
      };
      allowedSenders = mkOption {
        type = types.listOf types.str;
        default = [ ];
      };
    };

    openWebUI = {
      enable = mkEnableOption "Open WebUI memory bridge";
      baseUrl = mkOption {
        type = types.str;
        default = "http://127.0.0.1:8081";
      };
      apiKeyEnv = mkOption {
        type = types.str;
        default = "OPENWEBUI_API_KEY";
      };
    };

    authorizedKeys = mkOption {
      type = types.attrsOf (types.listOf types.str);
      default = { };
      example = {
        e-play = [ "ssh-ed25519 AAAA... ethan-desktop" ];
        e-work = [ "ssh-ed25519 AAAA... ethan-desktop" ];
      };
      description = "TUI client public keys, keyed by owner — the key file name is the session owner (per-user session isolation).";
    };
  };

  config = mkIf cfg.enable {
    users.users.${cfg.serviceUser} = {
      isSystemUser = true;
      group = cfg.serviceUser;
      home = cfg.stateDir;
      description = "temple agent service account";
    };
    users.groups.${cfg.serviceUser} = { };

    environment.etc = lib.mkMerge [
      {
        "temple/temple-daemon.toml" = {
          text = configText;
          mode = "0400";
        };
      }
      (listToAttrs (mapAttrsToList (owner: keys: {
        name = "temple/keys/${owner}";
        value = {
          text = concatStringsSep "\n" keys + "\n";
          mode = "0400";
        };
      }) cfg.authorizedKeys))
      (optionalAttrs (cfg.gitSafeDirectories != [ ]) {
        "temple/gitconfig".text = ''
          [safe]
          ${concatStringsSep "\n" (map (d: "\tdirectory = ${d}") cfg.gitSafeDirectories)}
        '';
      })
    ];

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0700 ${cfg.serviceUser} ${cfg.serviceUser} - -"
    ];

    systemd.services.temple-daemon = {
      description = "temple agent daemon (${cfg.serviceUser})";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment = {
        HOME = cfg.stateDir;
        GIT_CONFIG_GLOBAL = "/etc/temple/gitconfig";
      };
      environment.RUST_LOG = "temple_agent=info,temple_server=info";

      path = with pkgs; [
        bash
        coreutils
        gnugrep
        gnused
        findutils
        git
        ripgrep
        nix
        which
        gcc
        gnumake
        cargo
        rustc
      ];

      serviceConfig = {
        Type = "simple";
        User = cfg.serviceUser;
        Group = cfg.serviceUser;
        ExecStart = escapeShellArgs [
          "${cfg.package}/bin/temple"
          "--daemon"
          "--config"
          "/etc/temple/temple-daemon.toml"
        ];
        Restart = "always";
        RestartSec = "10s";
        StandardOutput = "journal";
        StandardError = "journal";

        SupplementaryGroups = cfg.supplementaryGroups;
        ReadWritePaths = [ cfg.stateDir ] ++ cfg.readWritePaths;

        NoNewPrivileges = true;
        ProtectSystem = "full";
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
        EnvironmentFile = cfg.environmentFile;
      });
    };
  };
}
