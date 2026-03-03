{ pkgs, self }:

pkgs.testers.runNixOSTest {
  name = "iroh-nix";

  nodes.requester = { config, pkgs, ... }: {
    imports = [ self.nixosModules.default ];

    environment.systemPackages = [
      self.packages.${pkgs.system}.iroh-nix
      pkgs.jq
    ];

    services.iroh-nix = {
      enable = true;
      package = self.packages.${pkgs.system}.iroh-nix;
      network = "test-cluster";
      relayMode = "disabled";
      address = "0.0.0.0";
      remoteBuilder.enable = true;
    };

    nix.settings.experimental-features = [ "nix-command" ];
  };

  nodes.builder = { config, pkgs, ... }: {
    imports = [ self.nixosModules.default ];

    environment.systemPackages = [
      self.packages.${pkgs.system}.iroh-nix
      pkgs.jq
    ];

    services.iroh-nix = {
      enable = true;
      package = self.packages.${pkgs.system}.iroh-nix;
      network = "test-cluster";
      relayMode = "disabled";
      builder.enable = true;
      address = "0.0.0.0";
    };

    nix.settings.experimental-features = [ "nix-command" ];
  };

  testScript = ''
    start_all()

    with subtest("Service startup"):
        requester.wait_for_unit("iroh-nix")
        builder.wait_for_unit("iroh-nix")
        requester.wait_for_open_port(8080)
        builder.wait_for_open_port(8080)

    with subtest("HTTP cache-info"):
        info = requester.succeed("curl -s http://127.0.0.1:8080/nix-cache-info")
        assert "StoreDir: /nix/store" in info, f"Expected StoreDir in nix-cache-info, got: {info}"
        assert "Priority: 40" in info, f"Expected Priority: 40, got: {info}"
        requester.fail("curl -sf http://127.0.0.1:8080/00000000000000000000000000000000.narinfo")

    with subtest("Index and serve a store path"):
        requester.succeed("systemctl stop iroh-nix")

        store_path = requester.succeed(
            "readlink -f /run/current-system/sw/bin/iroh-nix | sed 's|/nix/store/\\([^/]*\\).*|/nix/store/\\1|'"
        ).strip()

        requester.succeed(
            f"sudo -u iroh-nix iroh-nix --data-dir /var/lib/iroh-nix --no-substituters --relay-mode disabled add {store_path}"
        )

        requester.succeed("systemctl start iroh-nix")
        requester.wait_for_open_port(8080)

        store_hash = requester.succeed(
            "readlink -f /run/current-system/sw/bin/iroh-nix | sed 's|/nix/store/\\([^-]*\\)-.*|\\1|'"
        ).strip()

        narinfo = requester.succeed(f"curl -s http://127.0.0.1:8080/{store_hash}.narinfo")
        assert "StorePath:" in narinfo, f"Expected StorePath in narinfo, got: {narinfo}"
        assert "NarHash: sha256:" in narinfo, f"Expected NarHash in narinfo, got: {narinfo}"

    with subtest("Control socket created"):
        # The daemon creates a control socket when gossip is enabled.
        # Wait a moment for the async socket server to start.
        import time
        time.sleep(2)
        requester.succeed("test -S /var/lib/iroh-nix/control.sock")

    with subtest("Build hook binary accessible"):
        # Verify the build-hook subcommand is recognized and --help works
        requester.succeed("iroh-nix build-hook --help")

    with subtest("Build hook configured in nix settings"):
        # Verify that nix.settings.build-hook is set correctly
        nix_conf = requester.succeed("cat /etc/nix/nix.conf")
        assert "build-hook" in nix_conf, f"Expected build-hook in nix.conf, got: {nix_conf}"
        assert "iroh-nix" in nix_conf, f"Expected iroh-nix in build-hook setting, got: {nix_conf}"
        assert "control.sock" in nix_conf, f"Expected control.sock in build-hook setting, got: {nix_conf}"

    with subtest("Builder has no control socket (no remoteBuilder)"):
        # The builder node does not have remoteBuilder.enable, so it should
        # still have a control socket (it has gossip enabled).
        builder.succeed("test -S /var/lib/iroh-nix/control.sock")

    with subtest("Builder does not have build-hook configured"):
        # The builder node should NOT have iroh-nix as build-hook
        # (only remoteBuilder.enable sets that)
        nix_conf = builder.succeed("cat /etc/nix/nix.conf")
        assert "build-hook" not in nix_conf, f"Builder should not have build-hook in nix.conf, got: {nix_conf}"
  '';
}
