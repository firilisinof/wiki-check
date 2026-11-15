{
  description = "MCP service for a personal Markdown wiki kept in Git";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.systems.url = "github:nix-systems/triplet";

  outputs =
    {
      self,
      nixpkgs,
      systems,
    }:
    let
      eachSystem = f: nixpkgs.lib.genAttrs (import systems) (system: f nixpkgs.legacyPackages.${system});
    in
    {
      overlays.default = final: _: { wiki-mcp = final.callPackage ./package.nix { }; };
      packages = eachSystem (pkgs: rec {
        default = wiki-mcp;
        wiki-mcp = pkgs.callPackage ./package.nix { };
      });
      formatter = eachSystem (pkgs: pkgs.nixfmt);
    };
}
