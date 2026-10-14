{
  description = "Run and manage many Claude Code sessions from one place";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Must match GHOSTTY_COMMIT in the libghostty-vt-sys crate (Cargo.lock).
    ghostty-src = {
      url = "github:ghostty-org/ghostty/3425025e585a3403340d4dc6d65132f42e05e605";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      ghostty-src,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        claude-mux = pkgs.callPackage ./nix/package.nix { inherit ghostty-src; };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.claude-mux;
      });

      overlays.default = final: _prev: {
        claude-mux = final.callPackage ./nix/package.nix { inherit ghostty-src; };
      };


      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = [
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
