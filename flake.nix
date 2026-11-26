{
  description = "QueryMT development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-parts.url = "github:hercules-ci/flake-parts";
    querymt-desktop = {
      url = "github:querymt/querymt-desktop/0207452c0ce8a0b8ed78b34405a13a1ebdee44c4";
      flake = false;
    };
  };

  outputs = inputs @ {
    self,
    nixpkgs,
    rust-overlay,
    flake-parts,
    querymt-desktop,
    ...
  }: let
    pinnedDesktopRevision = builtins.replaceStrings ["\n" "\r"] ["" ""] (
      builtins.readFile ./crates/agent/embedded-ui-revision
    );
  in
    if querymt-desktop.rev != pinnedDesktopRevision
    then throw "querymt-desktop flake input ${querymt-desktop.rev} does not match crates/agent/embedded-ui-revision (${pinnedDesktopRevision})"
    else
      flake-parts.lib.mkFlake {inherit inputs;} {
        systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
        perSystem = {system, ...}: let
          overlays = [(import rust-overlay)];
          nixpkgsConfig = {
            allowUnfree = true;
          };

          pkgs = import nixpkgs {
            inherit system overlays;
            config = nixpkgsConfig;
          };

          pkgsCuda =
            if pkgs.stdenv.hostPlatform.isLinux
            then
              import nixpkgs {
                inherit system overlays;
                config =
                  nixpkgsConfig
                  // {
                    cudaSupport = true;
                    # CUDA 13 dropped these capabilities; the cuda shell pins cudaPackages_12.
                    cudaCapabilities = ["5.2" "6.0" "6.1" "7.0" "7.5" "8.0" "8.6"];
                  };
              }
            else null;

          rustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
            extensions = ["rust-src"];
          };

          workspaceCargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

          # typeshare 1.13.4 labels non-compilable sample output as a Rust doctest.
          # Keep its tests enabled and mark only that output example as text.
          patchedTypeshare = pkgs.typeshare.overrideAttrs (oldAttrs: {
            postPatch =
              (oldAttrs.postPatch or "")
              + ''
                substituteInPlace core/src/language/mod.rs \
                  --replace-fail $'/// ```rust\n    /// /// Generated type representing' \
                    $'/// ```text\n    /// /// Generated type representing'
              '';
          });

          commonInputs = with pkgs; [
            rustToolchain
            openssl
            cmake
            ninja
            gnumake
            nodejs
            dbus
            clang
            llvmPackages.libclang
            rust-bindgen
            pkg-config
            patchedTypeshare
            libgcc
            gdb
          ];

          dashboardPackageJson = builtins.fromJSON (builtins.readFile "${querymt-desktop}/package.json");
          dashboardUi = pkgs.buildNpmPackage {
            pname = "querymt-dashboard-ui";
            version = dashboardPackageJson.version;
            src = querymt-desktop;
            npmDeps = pkgs.importNpmLock {
              npmRoot = querymt-desktop;
            };
            npmConfigHook = pkgs.importNpmLock.npmConfigHook;
            preBuild = "npm run test:embedded";
            npmBuildScript = "build:embedded";
            QUERYMT_UI_REVISION = querymt-desktop.rev;
            nativeBuildInputs = [pkgs.jq];
            postBuild = ''
              test -f build-embedded/index.html
              test "$(jq -r .target build-embedded/querymt-ui.json)" = embedded
              test "$(jq -r .acpWebSocketPath build-embedded/querymt-ui.json)" = /acp/ws
              test "$(jq -r .revision build-embedded/querymt-ui.json)" = "$QUERYMT_UI_REVISION"
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out
              cp -R build-embedded/. $out/
              runHook postInstall
            '';
          };

          # Standalone ACP/MCP client binaries (crates/qmacp, crates/qmcp). They do
          # not use the agent stack, so they need no dashboard or bindgen setup.
          # The build sandbox has no .git, so build.rs reads the SHA from env.
          mkAcpClient = {
            name,
            shaVar,
          }:
            pkgs.rustPlatform.buildRustPackage {
              pname = name;
              version = workspaceCargoToml.workspace.package.version;
              src = ./.;
              cargoLock = {
                lockFile = ./Cargo.lock;
                allowBuiltinFetchGit = true;
              };
              cargoBuildFlags = [
                "-p"
                name
                "--bin"
                name
              ];
              nativeBuildInputs = [
                pkgs.pkg-config
                pkgs.cmake
              ];
              # rustPlatform sets AWS_LC_SYS_USE_SYSTEM=1; qmcp reaches aws-lc-sys
              # through reqwest's rustls.
              buildInputs = [pkgs.aws-lc];
              env.${shaVar} = self.shortRev or self.dirtyShortRev or "unknown";
              # See qmt below: cargo-auditable fails on this workspace's `dep:` features.
              auditable = false;
              # Tested by .github/workflows/test-qmacp.yml; workspace-level cargo
              # test in Nix is not scoped yet (see qmt below).
              doCheck = false;
              meta.mainProgram = name;
            };

          # wasm-bindgen CLI for the wasm builds (crates/querymt-gpui/web/build.sh).
          # Must match the `wasm-bindgen` crate version in Cargo.lock exactly
          # (querymt-gpui pins `=0.2.130`). Bump version + hashes together.
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

          # ─── QueryMT Desktop (crates/querymt-gpui) ─────────────────────────
          # Libraries GPUI loads at runtime on Linux (dlopen'd, so they must be on
          # LD_LIBRARY_PATH): Vulkan for rendering, Wayland/X11 for windowing,
          # xkbcommon for keyboard, fontconfig/freetype for text.
          gpuiRuntimeLibs = with pkgs;
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
          gpuiBuildLibs = gpuiRuntimeLibs ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [pkgs.alsa-lib pkgs.zstd];

          # crates/querymt-gpui/build.rs embeds pinned IBM Plex fonts and downloads
          # them when its cache is empty. The Nix sandbox has no network, so
          # pre-fetch the same files (same revision and SHA-256 as build.rs) into
          # the cache layout it reads: `$QMT_FONT_CACHE/<revision>/<file>`.
          plexRevision = "383c681f015ed2626e919ec4e3cca16ccc204e9d";
          plexFonts = [
            ["Sans" "IBMPlexSans-Regular.ttf" "975dcda37d80f038dcd143c22e33ca2d97a0cc5a929aace1c749153b0fe1afa5"]
            ["Sans" "IBMPlexSans-Medium.ttf" "331c8639d7598b2cde62a911a71db195e30cb655cd6bdf2e324a7e984955f907"]
            ["Sans" "IBMPlexSans-SemiBold.ttf" "a20caf8286023a6a7a85e40b1d2a4ae9fc3e3b1f9eda8f4c542dd4986af67bb1"]
            ["Sans" "IBMPlexSans-Bold.ttf" "9e6c74a889a700d707613d24548fe4ffa6bc59559a0689d2cf9e133bdcdafb2f"]
            ["Mono" "IBMPlexMono-Regular.ttf" "fe11304a5fe956d5744e9b6a246cc83d90425245e75a62230044966ca96a7f50"]
          ];
          plexFontCache = pkgs.linkFarm "querymt-gpui-font-cache" (map (font: let
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

          gpuiDesktopItem = pkgs.makeDesktopItem {
            name = "querymt-gpui";
            desktopName = "QueryMT";
            comment = "Native desktop control center for QueryMT";
            exec = "querymt-gpui %U";
            icon = "querymt-gpui";
            categories = ["Development"];
            # Matches the window's `app_id` so the shell groups it with this entry.
            startupWMClass = "querymt-gpui";
          };

          # The toolchain from rust-toolchain.toml, as the GPUI repository built it.
          toolchainRustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };

          querymtGpui = toolchainRustPlatform.buildRustPackage {
            pname = "querymt-gpui";
            version = workspaceCargoToml.workspace.package.version;
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = ["-p" "querymt-gpui" "--bin" "querymt-gpui"];

            nativeBuildInputs = with pkgs; [pkg-config makeWrapper copyDesktopItems];
            buildInputs = gpuiBuildLibs;
            desktopItems = [gpuiDesktopItem];

            # buildRustPackage always builds `--release`; give it the settings of
            # the workspace's `release-gpui` profile (size over speed).
            CARGO_PROFILE_RELEASE_OPT_LEVEL = "z";
            CARGO_PROFILE_RELEASE_LTO = "fat";
            CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "1";
            CARGO_PROFILE_RELEASE_PANIC = "abort";
            CARGO_PROFILE_RELEASE_STRIP = "symbols";

            preBuild = ''
              # build.rs writes into its cache directory, so give it a writable copy.
              export QMT_FONT_CACHE="$TMPDIR/qmt-fonts"
              cp -rL ${plexFontCache} "$QMT_FONT_CACHE"
              chmod -R u+w "$QMT_FONT_CACHE"
              export CARGO_NET_OFFLINE=true
            '';

            # See qmt below: cargo-auditable fails on this workspace's `dep:` features.
            auditable = false;
            # Tested by .github/workflows/test-gpui.yml (GPUI tests need a display
            # stack the sandbox does not have).
            doCheck = false;
            buildPhase = "cargoBuildHook";
            installPhase = ''
              runHook preInstall
              install -Dm755 target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/$cargoBuildType/querymt-gpui $out/bin/querymt-gpui
              install -Dm644 crates/querymt-gpui/assets/brand/querymt-stencil.svg \
                $out/share/icons/hicolor/scalable/apps/querymt-gpui.svg
              runHook postInstall
            '';

            # GPUI dlopens Vulkan, Wayland/X11 and xkbcommon at runtime.
            postFixup = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              wrapProgram $out/bin/querymt-gpui \
                --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath gpuiRuntimeLibs}"
            '';

            meta = {
              description = "QueryMT Desktop: the GPUI desktop client for QueryMT agents";
              homepage = "https://github.com/querymt/querymt";
              license = pkgs.lib.licenses.mit;
              mainProgram = "querymt-gpui";
              platforms = pkgs.lib.platforms.linux ++ pkgs.lib.platforms.darwin;
            };
          };
        in {
          packages = {
            dashboard-ui = dashboardUi;
            default = self.packages.${system}.qmt;

            qmtcode = pkgs.rustPlatform.buildRustPackage {
              pname = "qmtcode";
              version = workspaceCargoToml.workspace.package.version;
              src = ./.;
              cargoLock = {
                lockFile = ./Cargo.lock;
                allowBuiltinFetchGit = true;
              };
              cargoBuildFlags = [
                "-p"
                "querymt-agent"
                "--example"
                "qmtcode"
                "--features"
                "dashboard,oauth,zbus-secret-service,systemd-credentials,remote"
              ];
              nativeBuildInputs = [
                pkgs.pkg-config
                pkgs.cmake
                pkgs.gnumake
              ];
              buildInputs = commonInputs;
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.stdenv.cc.libc.dev}/include";
              QMT_UI_DIST = "${dashboardUi}";
              # Use the same bundled native libraries as a direct Cargo build.
              dontCargoUseSystemLibs = true;
              # cargo-auditable currently fails on this workspace's `dep:` feature
              # metadata resolution; we can re-enable it later if embedded
              # dependency metadata in binaries becomes a requirement.
              auditable = false;
              # Temporarily disable checkPhase: the current workspace-level cargo
              # test/check path is broken for this package in Nix and needs follow-up
              # to scope/fix checks before re-enabling.
              doCheck = false;
              buildPhase = "cargoBuildHook";
              installPhase = ''
                runHook preInstall
                mkdir -p $out/bin
                install -Dm755 target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/$cargoBuildType/examples/qmtcode $out/bin/qmtcode
                runHook postInstall
              '';
            };

            qmt = pkgs.rustPlatform.buildRustPackage {
              pname = "qmt";
              version = workspaceCargoToml.workspace.package.version;
              src = ./.;
              cargoLock = {
                lockFile = ./Cargo.lock;
                allowBuiltinFetchGit = true;
              };
              cargoBuildFlags = [
                "-p"
                "querymt-cli"
                "--bin"
                "qmt"
                "--features"
                "zbus-secret-service,systemd-credentials"
              ];
              nativeBuildInputs = [
                pkgs.pkg-config
                pkgs.cmake
                pkgs.gnumake
              ];
              buildInputs = commonInputs;
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.stdenv.cc.libc.dev}/include";
              # cargo-auditable currently fails on this workspace's `dep:` feature
              # metadata resolution; we can re-enable it later if embedded
              # dependency metadata in binaries becomes a requirement.
              auditable = false;
              # Temporarily disable checkPhase: the current workspace-level cargo
              # test/check path is broken for this package in Nix and needs follow-up
              # to scope/fix checks before re-enabling.
              doCheck = false;
              buildPhase = "cargoBuildHook";
              installPhase = ''
                runHook preInstall
                mkdir -p $out/bin
                install -Dm755 target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/$cargoBuildType/qmt $out/bin/qmt
                runHook postInstall
              '';
            };

            qmacp = mkAcpClient {
              name = "qmacp";
              shaVar = "QMACP_GIT_SHA";
            };
            qmcp = mkAcpClient {
              name = "qmcp";
              shaVar = "QMCP_GIT_SHA";
            };
            querymt-gpui = querymtGpui;
          };

          apps = {
            qmtcode = {
              type = "app";
              program = "${self.packages.${system}.qmtcode}/bin/qmtcode";
            };
            qmt = {
              type = "app";
              program = "${self.packages.${system}.qmt}/bin/qmt";
            };
            qmacp = {
              type = "app";
              program = "${self.packages.${system}.qmacp}/bin/qmacp";
            };
            qmcp = {
              type = "app";
              program = "${self.packages.${system}.qmcp}/bin/qmcp";
            };
            querymt-gpui = {
              type = "app";
              program = "${querymtGpui}/bin/querymt-gpui";
            };
          };

          devShells =
            {
              default = pkgs.mkShell {
                # wasm-bindgen-cli must match the locked wasm-bindgen crate (0.2.130) exactly.
                # The GPUI libraries let `cargo run -p querymt-gpui` work on Linux;
                # miniserve serves the desktop app's web build.
                buildInputs =
                  commonInputs
                  ++ gpuiBuildLibs
                  ++ [
                    pkgs.binaryen
                    pkgs.miniserve
                    wasmBindgenCli
                  ];

                LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath gpuiRuntimeLibs;

                shellHook =
                  /*
                  bash
                  */
                  ''
                    export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
                    export PS1="(env:querymt) $PS1"
                  '';
              };
            }
            // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
              vulkan = pkgs.mkShell {
                buildInputs = commonInputs ++ (with pkgs; [vulkan-loader]);

                shellHook =
                  /*
                  bash
                  */
                  ''
                    export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
                    export LIBRARY_PATH="/run/opengl-driver/lib:${pkgs.lib.makeLibraryPath (with pkgs; [gcc.cc.lib vulkan-loader])}:$LIBRARY_PATH"
                    export LD_LIBRARY_PATH="/run/opengl-driver/lib:${pkgs.lib.makeLibraryPath (with pkgs; [gcc.cc.lib vulkan-loader])}:$LD_LIBRARY_PATH"

                    export PS1="(env:querymt-vulkan) $PS1"
                  '';
              };

              cuda = pkgsCuda.mkShell {
                buildInputs =
                  commonInputs
                  ++ (with pkgsCuda; [
                    cudaPackages_12.cudatoolkit
                    cudaPackages_12.libcublas.static
                  ]);

                shellHook =
                  /*
                  bash
                  */
                  ''
                    export LIBCLANG_PATH="${pkgsCuda.llvmPackages.libclang.lib}/lib"
                    export CUDA_PATH="${pkgsCuda.cudaPackages_12.cudatoolkit}"
                    export LIBRARY_PATH="/run/opengl-driver/lib:${pkgsCuda.lib.makeLibraryPath (with pkgsCuda; [gcc.cc.lib cudaPackages_12.cudatoolkit])}:$LIBRARY_PATH"
                    export LD_LIBRARY_PATH="/run/opengl-driver/lib:${pkgsCuda.lib.makeLibraryPath (with pkgsCuda; [gcc.cc.lib cudaPackages_12.cudatoolkit])}:$LD_LIBRARY_PATH"
                    export RUSTFLAGS="-L native=$CUDA_PATH/lib -L native=${pkgsCuda.cudaPackages_12.libcublas.static}/lib"

                    export PS1="(env:querymt-cuda) $PS1"
                  '';
              };
            };
        };
      };
}
