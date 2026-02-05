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
      in {
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
