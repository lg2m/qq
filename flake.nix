{
  description = "QQ development and build environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    nixpkgs-x86-darwin.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";
    flake-parts.url = "github:hercules-ci/flake-parts";
    nub.url = "github:nubjs/nub";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      flake-parts,
      nixpkgs,
      rust-overlay,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        ./nix/packages.nix
        ./nix/dev-shells.nix
      ];

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      flake.overlays.default = nixpkgs.lib.composeManyExtensions [
        rust-overlay.overlays.default
        (import ./nix/overlay.nix)
      ];

      perSystem =
        { system, ... }:
        let
          nixpkgsFor = if system == "x86_64-darwin" then inputs.nixpkgs-x86-darwin else nixpkgs;
        in
        {
          _module.args.pkgs = import nixpkgsFor {
            inherit system;
            overlays = [ self.overlays.default ];
          };
        };
    };
}
