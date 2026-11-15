{
  description = "nixcache-oci";
  inputs = { nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable"; };
  outputs = { self, nixpkgs }: let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
  in {
    packages = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
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
  };
}