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
          };

          devShells =
            {
              default = pkgs.mkShell {
                buildInputs = commonInputs;

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
