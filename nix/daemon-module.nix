# temple-daemon NixOS module.
# Imported via the temple flake:
#   imports = [ temple.nixosModules.temple-daemon ];
#
# Creates systemd system services that start at boot (no login required).
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
    enable = mkEnableOption "temple headless daemons";

    package = mkOption {
      type = types.package;
      default = templePackage;
    };

    server = mkOption {
      type = types.str;
      default = "https://temple.ethanwtodd.com";
    };

    userDaemons = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "e-play" "e-work" ];
    };
  };

  config = mkIf cfg.enable {
    systemd.services = listToAttrs (map (name:
      nameValuePair "temple-daemon-${name}" {
        description = "temple headless daemon — ${name}";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];

        environment.HOME = "/home/${name}";
        environment.RUST_LOG = "temple_client=info";

        path = with pkgs; [
          bash
          coreutils
          gnugrep
          gnused
          findutils
          git
          ripgrep
        ];

        serviceConfig = {
          Type = "simple";
          User = name;
          Group = "users";
          ExecStart = escapeShellArgs [ "${cfg.package}/bin/temple" "--daemon" "--server" cfg.server "--identity" "/home/${name}/.ssh/id_ed25519" ];
          Restart = "always";
          RestartSec = "10s";
          StandardOutput = "journal";
          StandardError = "journal";

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
          RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
        };
      }
    ) cfg.userDaemons);
  };
}
