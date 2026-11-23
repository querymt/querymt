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
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # build.rs embeds pinned IBM Plex fonts and downloads them when its cache
        # is empty. The Nix sandbox has no network, so pre-fetch the same files
        # (same revision and SHA-256 as build.rs) into the cache layout it reads:
        # `$QMT_FONT_CACHE/<revision>/<file>`.
        plexRevision = "383c681f015ed2626e919ec4e3cca16ccc204e9d";
        plexFonts = [
          ["Sans" "IBMPlexSans-Regular.ttf" "975dcda37d80f038dcd143c22e33ca2d97a0cc5a929aace1c749153b0fe1afa5"]
          ["Sans" "IBMPlexSans-Medium.ttf" "331c8639d7598b2cde62a911a71db195e30cb655cd6bdf2e324a7e984955f907"]
          ["Sans" "IBMPlexSans-SemiBold.ttf" "a20caf8286023a6a7a85e40b1d2a4ae9fc3e3b1f9eda8f4c542dd4986af67bb1"]
          ["Sans" "IBMPlexSans-Bold.ttf" "9e6c74a889a700d707613d24548fe4ffa6bc59559a0689d2cf9e133bdcdafb2f"]
          ["Mono" "IBMPlexMono-Regular.ttf" "fe11304a5fe956d5744e9b6a246cc83d90425245e75a62230044966ca96a7f50"]
        ];
        fontCache = pkgs.linkFarm "querymt-gpui-font-cache" (map (font: let
            family = builtins.elemAt font 0;
            name = builtins.elemAt font 1;
          in {
            name = "${plexRevision}/${name}";
            path = pkgs.fetchurl {
              url = "https://raw.githubusercontent.com/IBM/plex/${plexRevision}/IBM-Plex-${family}/fonts/complete/ttf/${name}";
              sha256 = builtins.elemAt font 2;
            };
          })
          plexFonts);

        desktopItem = pkgs.makeDesktopItem {
          name = "querymt-gpui";
          desktopName = "QueryMT";
          comment = "Native desktop control center for QueryMT";
          exec = "querymt-gpui %U";
          icon = "querymt-gpui";
          categories = ["Development"];
          # Matches the window's `app_id` so the shell groups it with this entry.
          startupWMClass = "querymt-gpui";
        };

        querymt-gpui = rustPlatform.buildRustPackage {
          pname = "querymt-gpui";
          version = cargoToml.package.version;
          src = lib.cleanSource ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
            # One hash for the whole querymt/querymt checkout at the pinned
            # rev (Cargo.toml); refresh all three together when the rev moves.
            outputHashes = let
              querymt = "sha256-GNVgmbM+iypT0FzDCGyLbTcLKykFDtJBYpyVM0GOdMc=";
            in {
              "querymt-0.6.0" = querymt;
              "querymt-agent-protocol-types-0.6.0" = querymt;
              "querymt-agent-session-core-0.6.0" = querymt;
            };
          };

          nativeBuildInputs = with pkgs; [pkg-config makeWrapper copyDesktopItems];
          buildInputs = runtimeLibs ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [pkgs.alsa-lib pkgs.zstd];

          desktopItems = [desktopItem];

          preBuild = ''
            # build.rs writes into its cache directory, so give it a writable copy.
            export QMT_FONT_CACHE="$TMPDIR/qmt-fonts"
            cp -rL ${fontCache} "$QMT_FONT_CACHE"
            chmod -R u+w "$QMT_FONT_CACHE"
            export CARGO_NET_OFFLINE=true
          '';

          doCheck = false;

          postInstall = ''
            install -Dm644 assets/brand/querymt-stencil.svg \
              $out/share/icons/hicolor/scalable/apps/querymt-gpui.svg
          '';

          # GPUI dlopens Vulkan, Wayland/X11 and xkbcommon at runtime.
          postFixup = lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            wrapProgram $out/bin/querymt-gpui \
              --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}"
          '';

          meta = {
            description = "QueryMT GPUI desktop client";
            homepage = "https://github.com/querymt/querymt-gpui";
            license = lib.licenses.mit;
            mainProgram = "querymt-gpui";
            platforms = lib.platforms.linux ++ lib.platforms.darwin;
          };
        };
      in {
        packages = {
          default = querymt-gpui;
          querymt-gpui = querymt-gpui;
        };

        apps.default = {
          type = "app";
          program = "${querymt-gpui}/bin/querymt-gpui";
        };

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
