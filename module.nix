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

    substituters = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "https://cache.nixos.org" ];
      example = [ "https://cache.nixos.org" "https://my-company-cache.example.com" ];
      description = ''
        Upstream HTTP binary cache URLs for the pull-through cache.
        Set to an empty list to disable upstream fetching (local/P2P only).
      '';
    };

    narCacheSize = lib.mkOption {
      type = lib.types.str;
      default = "10%";
      example = "10G";
      description = ''
        Maximum NAR cache size. Supports suffixes: G, M, K for bytes,
        % for percentage of filesystem. "0" disables the limit.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Whether to open the firewall port for the HTTP cache.";
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
      after = [ "network.target" ];
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
        ++ (if cfg.substituters == [ ] then
          [ "--no-substituters" ]
        else
          lib.concatMap (s: [ "--substituter" s ]) cfg.substituters)
        ++ cfg.extraArgs
        ++ [
          "daemon"
          "--bind" "${cfg.address}:${toString cfg.port}"
          "--priority" (toString cfg.priority)
          "--nar-cache-size" cfg.narCacheSize
        ]);

        Restart = "on-failure";
        RestartSec = "5s";

        # Hardening
        NoNewPrivileges = true;
        ProtectHome = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ReadWritePaths = [ cfg.dataDir ];
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
  };
}
