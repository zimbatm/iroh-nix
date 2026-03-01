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

    relayUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "https://relay.iroh.network";
      description = "Relay URL for NAT traversal.";
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

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra command-line arguments to pass to iroh-nix.";
    };
  };

  config = lib.mkIf cfg.enable {
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
      after = [ "network.target" ] ++ lib.optional cfg.builder.enable "nix-daemon.service";
      requires = lib.optional cfg.builder.enable "nix-daemon.service";
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        User = "iroh-nix";
        Group = "iroh-nix";
        ExecStart = lib.escapeShellArgs ([
          "${cfg.package}/bin/iroh-nix"
          "--data-dir" cfg.dataDir
        ]
        ++ lib.optionals (cfg.relayUrl != null) [ "--relay-url" cfg.relayUrl ]
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
  };
}
