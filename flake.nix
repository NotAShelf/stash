{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
  }: let
    systems = ["x86_64-linux" "aarch64-linux"];
    forEachSystem = nixpkgs.lib.genAttrs systems;
    pkgsForEach = nixpkgs.legacyPackages;
  in {
    nixosModules = {
      stash = import ./nix/modules/nixos.nix self;
      default = self.nixosModules.stash;
    };

    packages = forEachSystem (system: let
      pkgs = pkgsForEach.${system};
      craneLib = crane.mkLib pkgs;
    in {
      stash = pkgs.callPackage ./nix/package.nix {inherit craneLib;};
      default = self.packages.${system}.stash;
    });

    checks = forEachSystem (system: let
      pkgs = pkgsForEach.${system};
    in {
      wayland = pkgs.callPackage ./nix/tests/wayland.nix {
        inherit (self.packages.${system}) stash;
      };
    });

    devShells = forEachSystem (system: {
      default = pkgsForEach.${system}.callPackage ./nix/shell.nix {};
    });

    hydraJobs = self.packages;
  };
}
