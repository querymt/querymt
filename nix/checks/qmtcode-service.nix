# Evaluates the qmtcode service module without building the Rust package.
{
  pkgs,
  self,
  system,
}: let
  lib = pkgs.lib;
  evalConfig = import (pkgs.path + "/nixos/lib/eval-config.nix");

  stub =
    pkgs.runCommand "qmtcode-stub" {
      meta.mainProgram = "qmtcode";
    } ''
      mkdir -p $out/bin
      printf '#!/bin/sh\nexit 0\n' > $out/bin/qmtcode
      chmod +x $out/bin/qmtcode
    '';

  base = {
    fileSystems."/" = {
      device = "nodev";
      fsType = "tmpfs";
    };
    boot.loader.grub.enable = false;
    system.stateVersion = "25.11";
  };

  eval = modules:
    evalConfig {
      inherit system modules;
    };

  nixos = eval [
    self.nixosModules.qmtcode
    base
    {
      services.qmtcode = {
        enable = true;
        package = lib.mkForce stub;
        user = "qmt";
        mode = "dashboard";
        bind = "0.0.0.0:3000";
        openFirewall = true;
        profile = "coder";
        profilesDir = ["%h/.qmt/profiles"];
        extraArgs = ["--mesh-iroh-gso=false"];
        mesh = {
          enable = true;
          bind = "/ip4/0.0.0.0/tcp/9001";
        };
      };
      users.users.qmt = {
        isNormalUser = true;
        home = "/home/qmt";
      };
    }
  ];

  conflict = eval [
    self.nixosModules.qmtcode
    base
    {
      services.qmtcode = {
        enable = true;
        package = lib.mkForce stub;
        profile = "coder";
        configFile = "/tmp/agent.toml";
      };
    }
  ];

  missingUser = eval [
    self.nixosModules.qmtcode
    base
    {
      services.qmtcode = {
        enable = true;
        package = lib.mkForce stub;
        user = "qmt-missing";
      };
    }
  ];

  home = eval [
    self.homeManagerModules.qmtcode
    base
    {
      services.qmtcode = {
        enable = true;
        package = lib.mkForce stub;
      };
    }
  ];

  failed = result: lib.filter (assertion: !assertion.assertion) result.config.assertions;

  assertNoFailures = name: result:
    lib.assertMsg (failed result == []) ''
      ${name} qmtcode module assertions failed:
      ${lib.concatMapStringsSep "\n" (assertion: assertion.message) (failed result)}
    '';

  assertHasFailure = name: needle: result:
    lib.assertMsg (lib.any (assertion: lib.hasInfix needle assertion.message) (failed result)) ''
      ${name} did not fail with "${needle}".
      failures:
      ${lib.concatMapStringsSep "\n" (assertion: assertion.message) (failed result)}
    '';

  unit = nixos.config.systemd.user.services.qmtcode;
  homeUnit = home.config.systemd.user.services.qmtcode;
  ports = nixos.config.networking.firewall.allowedTCPPorts;
  pathNames = map (pkg: pkg.pname or pkg.name) unit.path;
in
  assert assertNoFailures "nixos" nixos;
  assert assertHasFailure "configFile+profile" "mutually exclusive" conflict;
  assert assertHasFailure "missing user" "declared users.users" missingUser;
  assert lib.assertMsg nixos.config.users.users.qmt.linger "linger was not enabled";
  assert lib.assertMsg (unit.unitConfig.ConditionUser == "qmt") "ConditionUser was not set";
  assert lib.assertMsg (lib.elem 3000 ports && lib.elem 9001 ports) "firewall ports were ${toString ports}";
  assert lib.assertMsg (unit.environment.QMT_HOME == "%h/.qmt") "QMT_HOME was not the user state dir";
  assert lib.assertMsg (unit.environment.QMT_CONFIG_DIR == "%h/.qmt") "QMT_CONFIG_DIR was not set";
  assert lib.assertMsg (unit.environment.QMTCODE_MODE == "dashboard") "mode was not passed through";
  assert lib.assertMsg (unit.environment.QMTCODE_BIND == "0.0.0.0:3000") "bind was not passed through";
  assert lib.assertMsg (unit.environment.QMTCODE_PROFILE == "coder") "profile was not passed through";
  assert lib.assertMsg (unit.environment.QMTCODE_PROFILES_DIR_0 == "%h/.qmt/profiles") "profiles dir was not passed through";
  assert lib.assertMsg (unit.environment.QMTCODE_MESH == "1") "mesh was not enabled";
  assert lib.assertMsg (unit.environment.QMTCODE_MESH_BIND == "/ip4/0.0.0.0/tcp/9001") "mesh bind was not passed through";
  assert lib.assertMsg (unit.environment.HOSTNAME == "%H") "HOSTNAME specifier was not set";
  assert lib.assertMsg (unit.serviceConfig.WorkingDirectory == "%h") "working directory was not the user home";
  assert lib.assertMsg (lib.elem "git" pathNames && (lib.elem "bash" pathNames || lib.elem "bash-interactive" pathNames)) "service PATH is missing bash or git: ${toString pathNames}";
  assert lib.assertMsg (!lib.any (warning: lib.hasInfix "every user session" warning) nixos.config.warnings) "user warning fired despite services.qmtcode.user";
  assert lib.assertMsg (!(homeUnit.unitConfig ? ConditionUser)) "home-manager unit must not condition on a NixOS user";
  assert lib.assertMsg (homeUnit.environment.QMTCODE_MODE == "dashboard") "home-manager unit did not keep the dashboard default";
    pkgs.runCommand "qmtcode-service-module" {
      script = pkgs.writeText "qmtcode-service-script" unit.script;
    } ''
      grep -F -- '--dashboard="$QMTCODE_BIND"' "$script"
      grep -F -- '--profile' "$script"
      grep -F -- '--mesh="$QMTCODE_MESH_BIND"' "$script"
      grep -F -- '--mesh-iroh-gso=false' "$script"
      grep -F -- '${lib.getExe stub}' "$script"
      mkdir -p "$out"
      cp "$script" "$out/script"
    ''
