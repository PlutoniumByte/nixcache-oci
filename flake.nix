{
  description = "nixcache-oci: Self-hosted Nix binary cache via GHCR (OCI registry)";

  nixConfig = {
    extra-substituters = [ "http://localhost:37515" ];
    extra-trusted-public-keys = [ ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
  let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
  in {
    packages = forAllSystems (system:
    let pkgs = nixpkgs.legacyPackages.${system};
    in {
      cache-proxy = pkgs.rustPlatform.buildRustPackage {
        pname = "nixcache-proxy";
        version = "0.1.0";
        src = ./proxy;
        cargoLock.lockFile = ./proxy/Cargo.lock;
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.openssl ];
      };
      default = self.packages.${system}.cache-proxy;
    });

    apps = forAllSystems (system: {
      cache-proxy = {
        type = "app";
        program = "${self.packages.${system}.cache-proxy}/bin/nixcache-proxy";
      };
      default = self.apps.${system}.cache-proxy;
    });

    nixosModules.default = { config, pkgs, lib, ... }:
    let cfg = config.services.nixcache-proxy;
    in {
      options.services.nixcache-proxy = {
        enable = lib.mkEnableOption "nixcache-proxy OCI substituter bridge";
        repo = lib.mkOption {
          type = lib.types.str;
          default = "cmspam/nixcache-oci";
          description = "GitHub owner/repo hosting the OCI cache.";
        };
        port = lib.mkOption {
          type = lib.types.port;
          default = 37515;
          description = "Port the proxy listens on.";
        };
        listenAddress = lib.mkOption {
          type = lib.types.str;
          default = "127.0.0.1";
          description = "Address the proxy binds to.";
        };
        publicKey = lib.mkOption {
          type = lib.types.str;
          default = "";
          description = "Public key for verifying cache signatures.";
        };
        requireSignatures = lib.mkOption {
          type = lib.types.bool;
          default = true;
          description = "Whether to require valid signatures from this cache.";
        };
      };

      config = lib.mkIf cfg.enable {
        systemd.services.nixcache-proxy = {
          description = "Nix binary cache proxy for GHCR";
          wantedBy = [ "multi-user.target" ];
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];
          environment = {
            NIXCACHE_REPO = cfg.repo;
            NIXCACHE_PORT = toString cfg.port;
            NIXCACHE_LISTEN = cfg.listenAddress;
            NIXCACHE_INDEX_DIR = "/var/cache/nixcache-proxy";
          };
          serviceConfig = {
            ExecStart = "${self.packages.${pkgs.stdenv.hostPlatform.system}.cache-proxy}/bin/nixcache-proxy";
            Restart = "on-failure";
            DynamicUser = true;
            CacheDirectory = "nixcache-proxy";
            TimeoutStopSec = "10s";
          };
        };
        nix.settings = {
          extra-substituters = [ "http://localhost:${toString cfg.port}" ];
          extra-trusted-substituters = [ "http://localhost:${toString cfg.port}" ];
          extra-trusted-public-keys = lib.mkIf (cfg.publicKey != "") [ cfg.publicKey ];
          require-sigs = lib.mkIf (!cfg.requireSignatures) false;
          connect-timeout = 2;
          fallback = true;
        };
      };
    };
  };
}
