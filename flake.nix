{
  description = "Distributed Nix build system using iroh for P2P artifact distribution";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    {
      nixosModules.default = import ./module.nix;
      nixosModules.iroh-nix = self.nixosModules.default;

      overlays.default = final: prev: {
        iroh-nix = self.packages.${final.system}.iroh-nix;
      };
    } //
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Use latest stable Rust
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Common arguments for crane builds
        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;

          buildInputs = with pkgs; [
            openssl
            sqlite
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.darwin.apple_sdk.frameworks.Security
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
          ];

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
        };

        # Build the dependencies separately for caching
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Build the package
        iroh-nix = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });

      in
      {
        packages = {
          default = iroh-nix;
          inherit iroh-nix;
        };

        checks = {
          inherit iroh-nix;

          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          fmt = craneLib.cargoFmt {
            src = ./.;
          };

          test = craneLib.cargoTest (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};

          packages = with pkgs; [
            rustToolchain
            rust-analyzer
            cargo-watch
            cargo-edit
          ];

          RUST_LOG = "info";
        };
      }
    );
}
