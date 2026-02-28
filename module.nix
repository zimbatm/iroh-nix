# NixOS module for iroh-nix
{ config, lib, pkgs, ... }:

let
  cfg = config.services.iroh-nix;
  settingsFormat = pkgs.formats.toml { };
in
{
  options.services.iroh-nix = {
    enable = lib.mkEnableOption "iroh-nix distributed Nix build daemon";

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

    daemon = {
      enable = lib.mkEnableOption "iroh-nix daemon mode (serves NAR requests)";
    };

    builder = {
      enable = lib.mkEnableOption "iroh-nix builder mode";

      features = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "big-parallel" "kvm" ];
        description = "Additional features this builder supports.";
      };
    };

    substituter = {
      enable = lib.mkEnableOption "iroh-nix HTTP binary cache server";

      address = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
        description = "Address to bind the HTTP server to.";
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
        description = "Whether to open the firewall port for the substituter.";
      };
    };

    gc = {
      enable = lib.mkEnableOption "periodic garbage collection";

      minReplicas = lib.mkOption {
        type = lib.types.int;
        default = 1;
        description = "Minimum number of replicas required before deleting locally.";
      };

      gracePeriod = lib.mkOption {
        type = lib.types.int;
        default = 30;
        description = "Grace period in seconds before deleting after warning.";
      };

      maxDeletePerRun = lib.mkOption {
        type = lib.types.int;
        default = 100;
        description = "Maximum number of artifacts to delete per GC run.";
      };

      interval = lib.mkOption {
        type = lib.types.str;
        default = "daily";
        example = "hourly";
        description = "How often to run GC (systemd calendar expression).";
      };
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Extra command-line arguments to pass to iroh-nix.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.daemon.enable || cfg.builder.enable || cfg.substituter.enable || cfg.gc.enable;
        message = "At least one of services.iroh-nix.{daemon,builder,substituter,gc}.enable must be true";
      }
    ];

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

    # Daemon service
    systemd.services.iroh-nix-daemon = lib.mkIf cfg.daemon.enable {
      description = "iroh-nix daemon (NAR server)";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        User = "iroh-nix";
        Group = "iroh-nix";
        ExecStart = let
          args = [
            "${cfg.package}/bin/iroh-nix"
            "--data-dir" cfg.dataDir
          ]
          ++ lib.optionals (cfg.relayUrl != null) [ "--relay-url" cfg.relayUrl ]
          ++ lib.optionals (cfg.network != null) [ "--network" cfg.network ]
          ++ lib.concatMap (p: [ "--peer" p ]) cfg.peers
          ++ cfg.extraArgs
          ++ [ "daemon" ];
        in lib.escapeShellArgs args;
        Restart = "on-failure";
        RestartSec = "5s";

        # Hardening
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ReadWritePaths = [ cfg.dataDir ];
      };
    };

    # Builder service
    systemd.services.iroh-nix-builder = lib.mkIf cfg.builder.enable {
      description = "iroh-nix builder";
      after = [ "network.target" "nix-daemon.service" ];
      wantedBy = [ "multi-user.target" ];
      requires = [ "nix-daemon.service" ];

      serviceConfig = {
        Type = "simple";
        User = "iroh-nix";
        Group = "iroh-nix";
        ExecStart = let
          args = [
            "${cfg.package}/bin/iroh-nix"
            "--data-dir" cfg.dataDir
          ]
          ++ lib.optionals (cfg.relayUrl != null) [ "--relay-url" cfg.relayUrl ]
          ++ lib.optionals (cfg.network != null) [ "--network" cfg.network ]
          ++ lib.concatMap (p: [ "--peer" p ]) cfg.peers
          ++ cfg.extraArgs
          ++ [ "builder" ]
          ++ lib.concatMap (f: [ "--feature" f ]) cfg.builder.features;
        in lib.escapeShellArgs args;
        Restart = "on-failure";
        RestartSec = "5s";

        # Hardening (less restrictive since builder needs nix access)
        NoNewPrivileges = true;
        ProtectHome = true;
        PrivateTmp = true;
        ReadWritePaths = [ cfg.dataDir "/nix/store" "/nix/var" ];
      };
    };

    # Substituter service
    systemd.services.iroh-nix-substituter = lib.mkIf cfg.substituter.enable {
      description = "iroh-nix HTTP binary cache server";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        User = "iroh-nix";
        Group = "iroh-nix";
        ExecStart = let
          args = [
            "${cfg.package}/bin/iroh-nix"
            "--data-dir" cfg.dataDir
          ]
          ++ cfg.extraArgs
          ++ [
            "serve"
            "--bind" "${cfg.substituter.address}:${toString cfg.substituter.port}"
            "--priority" (toString cfg.substituter.priority)
          ];
        in lib.escapeShellArgs args;
        Restart = "on-failure";
        RestartSec = "5s";

        # Hardening
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ReadWritePaths = [ cfg.dataDir ];
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.substituter.openFirewall [
      cfg.substituter.port
    ];

    # Add builder to nix trusted users if builder mode is enabled
    nix.settings.trusted-users = lib.mkIf cfg.builder.enable [ "iroh-nix" ];

    # GC timer and service
    systemd.services.iroh-nix-gc = lib.mkIf cfg.gc.enable {
      description = "iroh-nix garbage collection";

      serviceConfig = {
        Type = "oneshot";
        User = "iroh-nix";
        Group = "iroh-nix";
        ExecStart = let
          args = [
            "${cfg.package}/bin/iroh-nix"
            "--data-dir" cfg.dataDir
          ]
          ++ lib.optionals (cfg.relayUrl != null) [ "--relay-url" cfg.relayUrl ]
          ++ lib.optionals (cfg.network != null) [ "--network" cfg.network ]
          ++ lib.concatMap (p: [ "--peer" p ]) cfg.peers
          ++ [
            "gc"
            "--min-replicas" (toString cfg.gc.minReplicas)
            "--grace-period" (toString cfg.gc.gracePeriod)
            "--max-delete" (toString cfg.gc.maxDeletePerRun)
          ];
        in lib.escapeShellArgs args;

        # Hardening
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ReadWritePaths = [ cfg.dataDir ];
      };
    };

    systemd.timers.iroh-nix-gc = lib.mkIf cfg.gc.enable {
      description = "iroh-nix periodic garbage collection";
      wantedBy = [ "timers.target" ];

      timerConfig = {
        OnCalendar = cfg.gc.interval;
        Persistent = true;
        RandomizedDelaySec = "5m";
      };
    };
  };
}
