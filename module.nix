# NixOS module for iroh-nix
{ config, lib, pkgs, ... }:

let
  cfg = config.services.iroh-nix;
in
{
  options.services.iroh-nix = {
    enable = lib.mkEnableOption "iroh-nix P2P Nix binary cache";

    package = lib.mkPackageOption pkgs "iroh-nix" { };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/iroh-nix";
      description = "Directory for storing blobs, index, and keys.";
    };

    relayMode = lib.mkOption {
      type = lib.types.str;
      default = "default";
      example = "disabled";
      description = "Relay mode: 'disabled', 'default', 'staging', or a relay URL.";
    };

    network = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "my-build-cluster";
      description = "Network ID for gossip discovery. Enables gossip when set.";
    };

    peers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "nodeid@192.168.1.10:11204" ];
      description = "Bootstrap peers for gossip discovery.";
    };

    # Substituter (always on when service is enabled)
    address = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = "Address to bind the HTTP binary cache server to.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8080;
      description = "Port for the HTTP binary cache server.";
    };

    priority = lib.mkOption {
      type = lib.types.int;
      default = 40;
      description = "Priority of this cache (lower = higher priority).";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Whether to open the firewall port for the HTTP cache.";
    };

    builder = {
      enable = lib.mkEnableOption "builder mode";

      features = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "big-parallel" "kvm" ];
        description = "Additional features this builder supports.";
      };

      streamLogs = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Stream build logs back to requester.";
      };
    };

    remoteBuilder = {
      enable = lib.mkEnableOption "transparent remote building via iroh network";

      exclusive = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Set max-jobs = 0 to route all builds through iroh (no local building).";
      };
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra command-line arguments to pass to iroh-nix.";
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [{
    users.users.iroh-nix = {
      isSystemUser = true;
      group = "iroh-nix";
      home = cfg.dataDir;
      description = "iroh-nix daemon user";
    };

    users.groups.iroh-nix = { };

    systemd.tmpfiles.rules = [
      "d ${cfg.dataDir} 0750 iroh-nix iroh-nix -"
    ];

    systemd.services.iroh-nix = {
      description = "iroh-nix P2P Nix binary cache";
      after = [ "network.target" ]
        ++ lib.optional (cfg.builder.enable || cfg.remoteBuilder.enable) "nix-daemon.service";
      requires = lib.optional (cfg.builder.enable || cfg.remoteBuilder.enable) "nix-daemon.service";
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        User = "iroh-nix";
        Group = "iroh-nix";
        ExecStart = lib.escapeShellArgs ([
          "${cfg.package}/bin/iroh-nix"
          "--data-dir" cfg.dataDir
          "--relay-mode" cfg.relayMode
        ]
        ++ lib.optionals (cfg.network != null) [ "--network" cfg.network ]
        ++ lib.concatMap (p: [ "--peer" p ]) cfg.peers
        ++ cfg.extraArgs
        ++ [
          "daemon"
          "--bind" "${cfg.address}:${toString cfg.port}"
          "--priority" (toString cfg.priority)
        ]
        ++ lib.optionals cfg.builder.enable [ "--builder" ]
        ++ lib.concatMap (f: [ "--feature" f ]) cfg.builder.features
        ++ lib.optionals cfg.builder.streamLogs [ "--stream-logs" ]);

        Restart = "on-failure";
        RestartSec = "5s";

        # Hardening -- relaxed when builder needs nix store access
        NoNewPrivileges = true;
        ProtectHome = true;
        PrivateTmp = true;
        ProtectSystem = if cfg.builder.enable then "full" else "strict";
        ReadWritePaths = [ cfg.dataDir ]
          ++ lib.optionals cfg.builder.enable [ "/nix/store" "/nix/var" ];
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [
      cfg.port
    ];

    # Always configure nix to use the local substituter
    nix.settings.substituters = [
      "http://${cfg.address}:${toString cfg.port}"
    ];
    nix.settings.trusted-substituters = [
      "http://${cfg.address}:${toString cfg.port}"
    ];

    # Add builder to nix trusted users if builder mode is enabled
    nix.settings.trusted-users = lib.mkIf cfg.builder.enable [ "iroh-nix" ];
  }

  # Remote builder configuration
  (lib.mkIf cfg.remoteBuilder.enable {
    # Set the build hook to use iroh-nix
    nix.settings.build-hook = "${cfg.package}/bin/iroh-nix build-hook --socket ${cfg.dataDir}/control.sock";

    # When exclusive mode is enabled, force all builds through iroh (no local building)
    nix.settings.max-jobs = lib.mkIf cfg.remoteBuilder.exclusive (lib.mkForce 0);
  })
  ]);
}
