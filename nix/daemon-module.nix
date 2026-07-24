# temple-daemon NixOS module.
# Imported via the temple flake:
#   imports = [ temple.nixosModules.temple-daemon ];
#
# Creates systemd system services that start at boot (no login required).
# Supports multiple daemons per host for multi-user desktops.
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
    enable = mkEnableOption "temple headless daemons — execute tool requests locally";

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

    userDaemons = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "e-play" "e-work" ];
      description = "System usernames to run daemons for.";
    };
  };

  config = mkIf cfg.enable (
    map (name:
      nameValuePair "temple-daemon-${name}" {
        description = "temple headless daemon — ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];

        environment.HOME = "/home/${name}";
        environment.RUST_LOG = "temple_client=info";

        serviceConfig = {
          Type = "simple";
          User = name;
          Group = "users";
          ExecStart = escapeShellArgs [
            "${cfg.package}/bin/temple"
            "--server"
            cfg.server
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
      }
    ) cfg.userDaemons
  );
}
