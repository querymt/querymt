{
  description = "QueryMT GPUI desktop prototype";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      systems = inputs.nixpkgs.lib.systems.flakeExposed;

      perSystem = {system, ...}: let
        overlays = [inputs.rust-overlay.overlays.default];
        pkgs = import inputs.nixpkgs {
          inherit system overlays;
        };
        lib = pkgs.lib;

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # Libraries GPUI loads at runtime on Linux (dlopen'd, so they must be on
        # LD_LIBRARY_PATH): Vulkan for rendering, Wayland/X11 for windowing,
        # xkbcommon for keyboard, fontconfig/freetype for text.
        runtimeLibs = with pkgs;
          lib.optionals stdenv.hostPlatform.isLinux [
            vulkan-loader
            wayland
            libxkbcommon
            libx11
            libxcb
            libxcursor
            libxrandr
            libxi
            fontconfig
            freetype
          ];
        # wasm-bindgen CLI must match the `wasm-bindgen` crate version in
        # Cargo.lock exactly. Bump version + hashes together when it changes.
        wasmBindgenCli = pkgs.buildWasmBindgenCli rec {
          src = pkgs.fetchCrate {
            pname = "wasm-bindgen-cli";
            version = "0.2.130";
            hash = "sha256-9Og1O/6/VC3sc5hBjoIWBErmLW2UHGMsTQO+6+m/dng=";
          };
          cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
            inherit src;
            inherit (src) pname version;
            hash = "sha256-j81sUf1m5yXU4FJfGj7GV+hdqVCV8mRmNb0QIsMCjqw=";
          };
        };
      in {
        devShells.default = pkgs.mkShell {
          packages =
            [
              rustToolchain
              pkgs.pkg-config
              pkgs.openssl
              # Web build: wasm -> JS glue, plus a static server for local testing.
              wasmBindgenCli
              pkgs.binaryen
              pkgs.miniserve
            ]
            ++ runtimeLibs
            ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.alsa-lib
              pkgs.zstd
            ];

          LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;

          shellHook = ''
            export PS1="(dev:gpui) $PS1"
          '';
        };
      };
    };
}
