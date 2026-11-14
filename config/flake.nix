{
  description = "nixcache-oci build configuration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nixcache.url = "github:PlutoniumByte/nixcache-oci";
  };

  outputs = { self, nixpkgs, nixcache }:
  let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
  in {
    packages = forAllSystems (system:
    let
      pkgs = nixpkgs.legacyPackages.${system}.extend (final: prev: {
        cardwire = prev.cardwire.overrideAttrs (oldAttrs: {
          buildInputs = (oldAttrs.buildInputs or [ ]) ++ [ final.sqlite ];
        });
      });
    in {
      default = pkgs.cardwire;
      nixcache-proxy = nixcache.packages.${system}.default;
    });
  };
}