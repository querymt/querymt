{
  description = "Querymt development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    flake-utils,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [(import rust-overlay)];
      pkgs = import nixpkgs {
        inherit system overlays;
      };

      rustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
        extensions = ["rust-src"];
      };
    in {
      # TODO: Add installation of qmt trough flake.
      # Can't be done as for now due to: https://github.com/NixOS/nixpkgs/issues/359340
      devShells.default = pkgs.mkShell {
        buildInputs = with pkgs; [
          rustToolchain
          openssl
          cmake
          libclang
          rust-bindgen
          pkg-config
        ];

        shellHook = ''
          export PS1="(env:querymt) $PS1"
        '';
      };
    });
}
