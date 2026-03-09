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
  '';
}
