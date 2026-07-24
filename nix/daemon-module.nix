# temple-daemon NixOS module.
# Imported via the temple flake:
#   imports = [ temple.nixosModules.temple-daemon ];
#
# Creates a systemd system service that starts at boot (no login required).
# Run one per user on each desktop machine.
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
in
{
  options.services.temple-daemon = {
    enable = mkEnableOption "temple headless daemon — executes tool requests locally";

    package = mkOption {
      type = types.package;
      default = templePackage;
      description = "The temple package providing the temple binary.";
    };

    server = mkOption {
      type = types.str;
      default = "https://temple.ethanwtodd.com";
      description = "Temple server WebSocket URL.";
    };

    user = mkOption {
      type = types.str;
      example = "e-play";
      description = "System user to run the daemon as.";
    };

    cwd = mkOption {
      type = types.str;
      example = "/home/e-play/Software";
      description = "Working directory for the daemon (tool execution scope).";
    };
  };

  config = mkIf cfg.enable {
    systemd.services."temple-daemon-${cfg.user}" = {
      description = "temple headless daemon — ${cfg.user}";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment.HOME = "/home/${cfg.user}";
      environment.RUST_LOG = "temple_client=info";

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = "users";
        ExecStart = escapeShellArgs [
          "${cfg.package}/bin/temple"
          "--server"
          cfg.server
          "--cwd"
          cfg.cwd
          "--daemon"
          "--identity"
          "%h/.ssh/id_ed25519"
        ];
        Restart = "always";
        RestartSec = "10s";
        StandardOutput = "journal";
        StandardError = "journal";

        NoNewPrivileges = true;
        ProtectSystem = "strict";
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
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
      };
    };
  };
}
