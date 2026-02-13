{
  description = "QueryMT development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs = inputs @ {
    self,
    nixpkgs,
    rust-overlay,
    flake-parts,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
      flake = let
        querymtServiceModule = import ./nix/nixos-module/querymt-service.nix {inherit self;};
      in {
        nixosModules = {
          querymt-service = querymtServiceModule;
          default = querymtServiceModule;
        };
      };
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
          if pkgs.stdenv.isLinux
          then
            import nixpkgs {
              inherit system overlays;
              config =
                nixpkgsConfig
                // {
                  cudaSupport = true;
                  cudaCapabilities = ["5.2" "6.0" "6.1" "7.0" "7.5" "8.0" "8.6"];
                  cudaVersion = "12";
                };
            }
          else null;

        rustToolchain = (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
          extensions = ["rust-src"];
        };

        agentCargoToml = builtins.fromTOML (builtins.readFile ./crates/agent/Cargo.toml);
        cliCargoToml = builtins.fromTOML (builtins.readFile ./crates/cli/Cargo.toml);
        serviceCargoToml = builtins.fromTOML (builtins.readFile ./crates/querymt-service/Cargo.toml);

        commonInputs = with pkgs; [
          rustToolchain
          openssl
          cmake
          ninja
          gnumake
          dbus
          clang
          llvmPackages.libclang
          rust-bindgen
          pkg-config
          libgcc
          gdb
        ];

        agentUi = pkgs.buildNpmPackage {
          pname = "qmt-agent-ui";
          version = agentCargoToml.package.version;
          src = ./crates/agent/ui;
          npmDepsHash = "sha256-EHiEmsoECK9+O56+zgbrfSR5hriFXYnS0VIZ0FMQY54=";
          npmBuildScript = "build";
          installPhase = ''
            runHook preInstall
            mkdir -p $out/dist
            cp -R dist/. $out/dist/
            runHook postInstall
          '';
        };
      in {
        packages = {
          agent-ui = agentUi;

          qmt-agent = pkgs.rustPlatform.buildRustPackage {
            pname = "qmt-agent";
            version = agentCargoToml.package.version;
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [
              "-p"
              "qmt-agent"
              "--example"
              "coder_agent"
              "--features"
              "dashboard,oauth,dbus-secret-service"
            ];
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake
              pkgs.gnumake
            ];
            buildInputs = commonInputs;
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.stdenv.cc.libc.dev}/include";
            QMT_UI_DIST = "${agentUi}/dist";
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
              install -Dm755 target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/$cargoBuildType/examples/coder_agent $out/bin/qmt-agent
              runHook postInstall
            '';
          };

          qmt = pkgs.rustPlatform.buildRustPackage {
            pname = "qmt";
            version = cliCargoToml.package.version;
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

          qmt-service = pkgs.rustPlatform.buildRustPackage {
            pname = "qmt-service";
            version = serviceCargoToml.package.version;
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [
              "-p"
              "querymt-service"
              "--bin"
              "qmt-service"
            ];
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake
              pkgs.gnumake
            ];
            buildInputs = commonInputs;
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.stdenv.cc.libc.dev}/include";
            auditable = false;
            doCheck = false;
            buildPhase = "cargoBuildHook";
            installPhase = ''
              runHook preInstall
              mkdir -p $out/bin
              install -Dm755 target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/$cargoBuildType/qmt-service $out/bin/qmt-service
              runHook postInstall
            '';
          };
        };

        apps = {
          qmt-agent = {
            type = "app";
            program = "${self.packages.${system}.qmt-agent}/bin/qmt-agent";
          };
          qmt = {
            type = "app";
            program = "${self.packages.${system}.qmt}/bin/qmt";
          };
          qmt-service = {
            type = "app";
            program = "${self.packages.${system}.qmt-service}/bin/qmt-service";
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
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
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
                  cudaPackages.cudatoolkit
                  cudaPackages.libcublas.static
                ]);

              shellHook =
                /*
                bash
                */
                ''
                  export LIBCLANG_PATH="${pkgsCuda.llvmPackages.libclang.lib}/lib"
                  export CUDA_PATH="${pkgsCuda.cudaPackages.cudatoolkit}"
                  export LIBRARY_PATH="/run/opengl-driver/lib:${pkgsCuda.lib.makeLibraryPath (with pkgsCuda; [gcc.cc.lib cudaPackages.cudatoolkit])}:$LIBRARY_PATH"
                  export LD_LIBRARY_PATH="/run/opengl-driver/lib:${pkgsCuda.lib.makeLibraryPath (with pkgsCuda; [gcc.cc.lib cudaPackages.cudatoolkit])}:$LD_LIBRARY_PATH"
                  export RUSTFLAGS="-L native=$CUDA_PATH/lib -L native=${pkgsCuda.cudaPackages.libcublas.static}/lib"

                  export PS1="(env:querymt-cuda) $PS1"
                '';
            };
          };
      };
    };
}
