# temple-daemon NixOS module — full per-user agent services.
# Imported via the temple flake:
#   imports = [ temple.nixosModules.temple-daemon ];
#
# Creates systemd SYSTEM services that start at boot (no login required),
# one per configured user, each running the complete agent: loop, tools
# (executed in-process), sessions, cron, and — on the daemon marked as
# owning Signal — the shared Signal presence. The TUI connects to the
# local daemon's WebSocket (pubkey auth via /etc/temple/keys).
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

  modelLines = m: [
    "default_model = \"${m.defaultModel}\""
    "simple_model = \"${m.simpleModel}\""
    "planner_model = \"${m.plannerModel}\""
    "executor_model = \"${m.executorModel}\""
    "reviewer_model = \"${m.reviewerModel}\""
    "researcher_model = \"${m.researcherModel}\""
    "router_model = \"${if m.routerModel != null then m.routerModel else m.researcherModel}\""
    "title_model = \"${if m.titleModel != null then m.titleModel else m.researcherModel}\""
  ];

  backendLines = mapAttrsToList (name: url: ''  "${name}" = "${url}"'') cfg.modelEndpoints;

  # One TOML config file per user (the daemon's --config).
  mkConfig = name: idx: concatStringsSep "\n" (
    [
      "listen = \"127.0.0.1:${toString (cfg.listenBasePort + idx)}\""
      "listen_health = \"127.0.0.1:${toString (cfg.listenBasePort + idx + 100)}\""
      "db_path = \"${cfg.stateDir}/${name}/temple.db\""
      "authorized_keys_dir = \"/etc/temple/keys\""
      "searxng_url = \"${cfg.searxngUrl}\""
      "default_permission = \"${cfg.defaultPermission}\""
      "allowed_dirs = ["
      (concatMapStringsSep ",\n" (d: "  \"${d}\"") cfg.allowedDirs)
      "]"
    ]
    ++ optional (cfg.authTokenFile != null) "auth_token_file = \"${toString cfg.authTokenFile}\""
    ++ optional (cfg.signal.enable && name == cfg.signal.owner) (
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
    ++ [
      ""
      "[models]"
    ]
    ++ modelLines cfg
    ++ optional (cfg.modelEndpoints != { }) (
      concatStringsSep "\n" ([ "backends = {" ] ++ backendLines ++ [ "}" ])
    )
  );
in
{
  options.services.temple-daemon = {
    enable = mkEnableOption "temple full-agent daemons";

    package = mkOption {
      type = types.package;
      default = templePackage;
    };

    stateDir = mkOption {
      type = types.str;
      default = "/var/lib/temple";
      description = "Per-user state: DBs, session logs.";
    };

    userDaemons = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "e-play" "e-work" ];
      description = "System usernames to run a full agent for.";
    };

    listenBasePort = mkOption {
      type = types.port;
      default = 42123;
      description = "First daemon WebSocket port; each user gets basePort + index.";
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

    authTokenFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Auth token file for Signal /verify registration (Signal-owning daemon).";
    };

    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "EnvironmentFile for secrets (e.g. OPENWEBUI_API_KEY).";
    };

    signal = {
      enable = mkEnableOption "Signal presence (one daemon owns the shared number)";
      owner = mkOption {
        type = types.str;
        default = "";
        description = "Username whose daemon owns the shared Signal number.";
      };
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
      };
      description = "TUI client public keys, keyed by owner (pubkey auth on the local WebSocket).";
    };
  };

  config = mkIf cfg.enable {
    environment.etc = lib.mkMerge [
      (listToAttrs (imap0 (idx: name: {
        name = "temple/daemon-${name}.toml";
        value = {
          text = mkConfig name idx;
          mode = "0400";
        };
      }) cfg.userDaemons))
      (listToAttrs (mapAttrsToList (owner: keys: {
        name = "temple/keys/${owner}";
        value = {
          text = concatStringsSep "\n" keys + "\n";
          mode = "0400";
        };
      }) cfg.authorizedKeys))
    ];

    systemd.tmpfiles.rules = map (name: "d ${cfg.stateDir}/${name} 0700 ${name} users - -") cfg.userDaemons;

    systemd.services = listToAttrs (imap0 (idx: name:
      nameValuePair "temple-daemon-${name}" {
        description = "temple agent daemon — ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];

        environment.HOME = "/home/${name}";
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
          User = name;
          Group = "users";
          ExecStart = escapeShellArgs [
            "${cfg.package}/bin/temple"
            "--daemon"
            "--config"
            "/etc/temple/daemon-${name}.toml"
          ];
          Restart = "always";
          RestartSec = "10s";
          StandardOutput = "journal";
          StandardError = "journal";

          ReadWritePaths = [ cfg.stateDir ];

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
      }
    ) cfg.userDaemons);
  };
}
