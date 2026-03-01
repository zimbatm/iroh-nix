{ pkgs, self }:

pkgs.testers.runNixOSTest {
  name = "iroh-nix";

  nodes.machine = { config, pkgs, ... }: {
    imports = [ self.nixosModules.default ];

    environment.systemPackages = [ self.packages.${pkgs.system}.iroh-nix ];

    services.iroh-nix = {
      enable = true;
      package = self.packages.${pkgs.system}.iroh-nix;
      daemon.enable = true;
      substituter.enable = true;
    };
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    with subtest("Service startup"):
        machine.wait_for_unit("iroh-nix-daemon")
        machine.wait_for_unit("iroh-nix-substituter")

        # Verify user and group exist
        machine.succeed("id iroh-nix")
        machine.succeed("getent group iroh-nix")

        # Verify data directory exists with correct ownership
        machine.succeed("test -d /var/lib/iroh-nix")
        machine.succeed("stat -c '%U:%G' /var/lib/iroh-nix | grep -q 'iroh-nix:iroh-nix'")

    with subtest("nix-cache-info endpoint"):
        machine.wait_for_open_port(8080)
        info = machine.succeed("curl -s http://127.0.0.1:8080/nix-cache-info")
        assert "StoreDir: /nix/store" in info, f"Expected StoreDir in nix-cache-info, got: {info}"
        assert "Priority: 40" in info, f"Expected Priority: 40, got: {info}"
        assert "WantMassQuery: 1" in info, f"Expected WantMassQuery: 1, got: {info}"

        # Unknown narinfo should return 404
        machine.fail("curl -sf http://127.0.0.1:8080/0000000000000000000000000000000.narinfo")

    with subtest("Index a store path"):
        # Stop services to avoid SQLite contention and iroh port conflicts
        machine.succeed("systemctl stop iroh-nix-daemon iroh-nix-substituter")

        # Pick a store path that definitely exists: the iroh-nix binary itself
        store_path = machine.succeed(
            "readlink -f /run/current-system/sw/bin/iroh-nix | sed 's|/nix/store/\\([^/]*\\).*|/nix/store/\\1|'"
        ).strip()

        # Index it as the iroh-nix user
        machine.succeed(
            f"sudo -u iroh-nix iroh-nix --data-dir /var/lib/iroh-nix --no-substituters add {store_path}"
        )

        # Restart only the substituter for the narinfo lookup test
        machine.succeed("systemctl start iroh-nix-substituter")

    with subtest("Narinfo lookup"):
        machine.wait_for_open_port(8080)

        # Extract the nix store hash from the indexed path
        store_hash = machine.succeed(
            "readlink -f /run/current-system/sw/bin/iroh-nix | sed 's|/nix/store/\\([^-]*\\)-.*|\\1|'"
        ).strip()

        narinfo = machine.succeed(f"curl -s http://127.0.0.1:8080/{store_hash}.narinfo")
        assert "StorePath:" in narinfo, f"Expected StorePath in narinfo, got: {narinfo}"
        assert "NarHash: sha256:" in narinfo, f"Expected NarHash in narinfo, got: {narinfo}"
        assert "NarSize:" in narinfo, f"Expected NarSize in narinfo, got: {narinfo}"
        assert store_path in narinfo, f"Expected {store_path} in narinfo, got: {narinfo}"
        assert "References:" in narinfo, f"Expected References in narinfo, got: {narinfo}"
  '';
}
