{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";

  outputs = {
    self,
    nixpkgs,
    crane,
  }: let
    systems = ["x86_64-linux" "aarch64-linux"];
    forEachSystem = nixpkgs.lib.genAttrs systems;
    pkgsForEach = nixpkgs.legacyPackages;
  in {
    packages = forEachSystem (system: {
      default = pkgsForEach.${system}.callPackage ./nix/package.nix {
        craneLib = crane.mkLib pkgsForEach.${system};
      };
    });

    devShells = forEachSystem (system: {
      default = pkgsForEach.${system}.callPackage ./nix/shell.nix {};
    });

    hydraJobs = self.packages;
  };
}
