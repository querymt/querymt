# User service for the existing qmtcode package.
#
# This does not package provider plugins or add a headless secret backend.
# API keys still come from the user's Secret Service session. A graphical
# login, or an already unlocked keyring, is required before the service can
# read them. Linger only keeps the user manager running; it does not unlock
# the keyring.
{
  self,
  scope,
}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.qmtcode;
  nixos = scope == "nixos";

  packageForSystem = let
    system = pkgs.stdenv.hostPlatform.system;
  in
    if self.packages ? ${system}
    then self.packages.${system}.qmtcode
    else
      throw ''
        qmtcode has no package for ${system}.
        Set services.qmtcode.package explicitly.
      '';

  meshOn = cfg.mode == "mesh" || cfg.mesh.enable;

  tcpPort = addr: let
    matched = builtins.match ".*:([0-9]+)$" addr;
  in
    if matched == null
    then null
    else lib.toInt (builtins.head matched);

  meshTcpPort = addr: let
    matched = builtins.match ".*/tcp/([0-9]+).*" addr;
  in
    if matched == null
    then null
    else lib.toInt (builtins.head matched);

  openable = port: port != null && port > 0 && port < 65536;

  loopback = addr:
    lib.hasPrefix "127." addr
    || lib.hasPrefix "localhost:" addr
    || addr == "localhost"
    || lib.hasPrefix "[::1]" addr
    || lib.hasPrefix "::1:" addr;

  servicePort =
    if cfg.mode == "mesh" || loopback cfg.bind
    then null
    else tcpPort cfg.bind;

  lanMeshPort =
    if meshOn && !cfg.mesh.noLan
    then meshTcpPort cfg.mesh.bind
    else null;

  firewallPorts = lib.optionals cfg.openFirewall (
    lib.unique (
      lib.optional (openable servicePort) servicePort
      ++ lib.optional (openable lanMeshPort) lanMeshPort
    )
  );

  profileDirEnv = lib.listToAttrs (lib.imap0 (i: dir: {
      name = "QMTCODE_PROFILES_DIR_${toString i}";
      value = toString dir;
    })
    cfg.profilesDir);

  serviceEnv =
    {
      QMT_HOME = cfg.stateDir;
      QMT_CONFIG_DIR = cfg.stateDir;
      QMTCODE_MODE = cfg.mode;
      QMTCODE_BIND = cfg.bind;
      QMTCODE_MESH =
        if meshOn
        then "1"
        else "0";
      QMTCODE_MESH_BIND = cfg.mesh.bind;
      QMTCODE_MESH_NO_LAN =
        if cfg.mesh.noLan
        then "1"
        else "0";
      # Mesh node naming falls back to the hostname binary. A user service
      # PATH does not include it, and a missing binary becomes "unknown".
      HOSTNAME = "%H";
    }
    // lib.optionalAttrs (cfg.configFile != null) {
      QMTCODE_CONFIG_FILE = toString cfg.configFile;
    }
    // lib.optionalAttrs (cfg.profile != null) {
      QMTCODE_PROFILE = cfg.profile;
    }
    // lib.optionalAttrs (cfg.db != null) {
      QMT_SESSIONS_DB = toString cfg.db;
    }
    // profileDirEnv
    // cfg.environment;
in {
  options.services.qmtcode =
    {
      enable = lib.mkEnableOption ''
        the qmtcode user service.

        The unit runs as the selected user so it can use that account's
        ~/.qmt state and D-Bus Secret Service. It does not install provider
        plugins; those are still loaded from the user's provider config or
        pulled from their configured OCI registry.
      '';

      package = lib.mkOption {
        type = lib.types.package;
        default = packageForSystem;
        defaultText = lib.literalExpression "querymt.packages.\${system}.qmtcode";
        description = "qmtcode package to run.";
      };

      mode = lib.mkOption {
        type = lib.types.enum ["dashboard" "api" "acp-ws" "mesh"];
        default = "dashboard";
        description = ''
          Exclusive transport. `qmtcode` accepts one of `--dashboard`,
          `--api`, or `--acp-ws`, and `--mesh` is separate. `mesh` means no
          transport: only the mesh node. To serve a UI and mesh at once, keep
          `mode` as `dashboard` (or `api` / `acp-ws`) and set
          {option}`services.qmtcode.mesh.enable`.
        '';
        example = "dashboard";
      };

      bind = lib.mkOption {
        type = lib.types.str;
        default =
          if config.services.qmtcode.mode == "acp-ws"
          then "127.0.0.1:3030"
          else "127.0.0.1:3000";
        defaultText = lib.literalExpression ''
          "127.0.0.1:3030" for acp-ws, otherwise "127.0.0.1:3000"
        '';
        example = "0.0.0.0:42069";
        description = ''
          Address passed to `--dashboard`, `--api`, or `--acp-ws`.
          systemd specifiers such as `%h` are expanded. Ignored when
          {option}`services.qmtcode.mode` is `mesh`.
        '';
      };

      stateDir = lib.mkOption {
        type = lib.types.str;
        default = "%h/.qmt";
        example = "%h/.qmt";
        description = ''
          Value of `QMT_HOME` and `QMT_CONFIG_DIR`. The default is the user's
          existing QueryMT directory, so sessions, profiles, mesh identity, and
          provider config stay where the CLI already looks. systemd specifiers
          are expanded. This directory is created on start.
        '';
      };

      configFile = lib.mkOption {
        type = lib.types.nullOr (lib.types.either lib.types.path lib.types.str);
        default = null;
        description = ''
          Agent config passed as qmtcode's positional TOML path. Mutually
          exclusive with {option}`services.qmtcode.profile`. A string may
          contain systemd specifiers.
        '';
      };

      profile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "coder";
        description = ''
          Profile id passed as `--profile`. When unset, qmtcode loads its
          embedded `default` profile. Mutually exclusive with
          {option}`services.qmtcode.configFile`.
        '';
      };

      profilesDir = lib.mkOption {
        type = lib.types.listOf (lib.types.either lib.types.path lib.types.str);
        default = [];
        description = ''
          Extra profile directories, passed as repeated `--profiles-dir`.
          String entries may contain systemd specifiers.
        '';
      };

      db = lib.mkOption {
        type = lib.types.nullOr (lib.types.either lib.types.path lib.types.str);
        default = null;
        example = "%h/.qmt/sessions.db";
        description = ''
          Sessions database passed as `--db`. When unset, qmtcode uses
          `$QMT_HOME/sessions.db`. A string may contain systemd specifiers.
        '';
      };

      workingDirectory = lib.mkOption {
        type = lib.types.str;
        default = "%h";
        example = "%h/src";
        description = ''
          Working directory for the service. Agent-relative paths resolve
          against this. systemd specifiers are expanded.
        '';
      };

      path = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = with pkgs; [bash coreutils git];
        defaultText = lib.literalExpression "[ pkgs.bash pkgs.coreutils pkgs.git ]";
        example = lib.literalExpression "[ pkgs.bash pkgs.git pkgs.python3 pkgs.nodejs ]";
        description = ''
          Packages placed on the service `PATH`. A user service does not
          inherit `/run/current-system/sw/bin`. The shell tool and hooks need
          `sh`; some worktree paths still invoke `git`. Add `python3` and
          `nodejs` when profiles use slash-command scripts or MCP servers.
        '';
      };

      environment = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = {};
        example = {
          QMT_NO_TELEMETRY = "1";
        };
        description = ''
          Extra environment variables. Do not put API keys here; qmtcode reads
          those from the user Secret Service. Values may contain systemd
          specifiers.
        '';
      };

      extraArgs = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        description = ''
          Extra qmtcode arguments. These are literal: systemd specifiers are
          not expanded. Do not put invite tokens or other secrets here; they
          would be stored in the unit and in the journal.
        '';
      };

      mesh = {
        enable = lib.mkOption {
          type = lib.types.bool;
          default = false;
          description = ''
            Add `--mesh` to the selected transport. This is not an alternative
            to {option}`services.qmtcode.mode`. For
            `qmtcode --dashboard=0.0.0.0:42069 --mesh=/ip4/0.0.0.0/tcp/9001`
            set `mode = "dashboard"`, `bind = "0.0.0.0:42069"`,
            `mesh.enable = true`, and `mesh.bind` to that multiaddr.
            Implied when {option}`services.qmtcode.mode` is `mesh`.
          '';
        };

        bind = lib.mkOption {
          type = lib.types.str;
          default = "/ip4/0.0.0.0/tcp/0";
          example = "/ip4/0.0.0.0/tcp/9001";
          description = ''
            LAN multiaddr passed to `--mesh`. Port `0` asks the kernel for a
            free port and cannot be opened in the firewall.
          '';
        };

        noLan = lib.mkOption {
          type = lib.types.bool;
          default = false;
          description = ''
            Pass `--mesh-no-lan`. The node only reconnects stored Iroh
            memberships and does not use mDNS.
          '';
        };
      };
    }
    // lib.optionalAttrs nixos {
      user = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "ivan";
        description = ''
          Account that should run qmtcode. When set, the unit is conditioned
          on that user and lingering is enabled so the user manager starts at
          boot. Lingering does not unlock the Secret Service keyring. When
          unset, the unit is installed for every user session. Home Manager
          does not have this option; that import already runs as one user.
        '';
      };

      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Open the dashboard, API, or ACP port, and the mesh TCP port when it
          is fixed and not zero. Localhost binds are not opened. Iroh uses UDP,
          which this option does not open.
        '';
      };
    };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      assertions = [
        {
          assertion = !(cfg.configFile != null && cfg.profile != null);
          message = "services.qmtcode.configFile and services.qmtcode.profile are mutually exclusive.";
        }
        {
          assertion = cfg.stateDir != "";
          message = "services.qmtcode.stateDir must not be empty.";
        }
        {
          assertion = cfg.mode == "mesh" || cfg.bind != "";
          message = "services.qmtcode.bind must not be empty.";
        }
        {
          assertion = !cfg.mesh.noLan || meshOn;
          message = "services.qmtcode.mesh.noLan requires mesh.enable or mode = \"mesh\".";
        }
      ];

      systemd.user.services.qmtcode = {
        description = "QueryMT coding agent";
        documentation = ["https://docs.query.mt"];
        wantedBy = ["default.target"];
        wants = ["network-online.target"];
        after = ["network-online.target" "dbus.socket"];
        environment = serviceEnv;
        path = cfg.path;
        serviceConfig = {
          WorkingDirectory = cfg.workingDirectory;
          Restart = "on-failure";
          RestartSec = 5;
          TimeoutStopSec = 20;
          KillMode = "mixed";
          SyslogIdentifier = "qmtcode";
        };
        # Specifiers are expanded in Environment= and WorkingDirectory=, not
        # inside this script text. The script therefore reads the expanded
        # variables instead of embedding paths.
        script = ''
          set -eu
          mkdir -p "$QMT_HOME"
          if [ -n "''${QMT_SESSIONS_DB:-}" ]; then
            mkdir -p "$(dirname "$QMT_SESSIONS_DB")"
          fi
          args=()
          if [ -n "''${QMTCODE_CONFIG_FILE:-}" ]; then
            args+=("$QMTCODE_CONFIG_FILE")
          fi
          if [ -n "''${QMTCODE_PROFILE:-}" ]; then
            args+=(--profile "$QMTCODE_PROFILE")
          fi
          i=0
          while true; do
            var="QMTCODE_PROFILES_DIR_$i"
            value="''${!var-}"
            if [ -z "$value" ]; then
              break
            fi
            args+=(--profiles-dir "$value")
            i=$((i + 1))
          done
          if [ -n "''${QMT_SESSIONS_DB:-}" ]; then
            args+=(--db "$QMT_SESSIONS_DB")
          fi
          case "$QMTCODE_MODE" in
            dashboard) args+=(--dashboard="$QMTCODE_BIND") ;;
            api) args+=(--api="$QMTCODE_BIND") ;;
            acp-ws) args+=(--acp-ws="$QMTCODE_BIND") ;;
            mesh) ;;
            *)
              echo "unsupported qmtcode mode: $QMTCODE_MODE" >&2
              exit 1
              ;;
          esac
          if [ "$QMTCODE_MESH" = 1 ]; then
            args+=(--mesh="$QMTCODE_MESH_BIND")
            if [ "$QMTCODE_MESH_NO_LAN" = 1 ]; then
              args+=(--mesh-no-lan)
            fi
          fi
          ${lib.optionalString (cfg.extraArgs != []) ''
            args+=(${lib.escapeShellArgs cfg.extraArgs})
          ''}
          exec ${lib.escapeShellArg (lib.getExe cfg.package)} "''${args[@]}"
        '';
      };
    }
    (lib.mkIf nixos {
      warnings =
        lib.optional (cfg.user == null) ''
          services.qmtcode.user is unset, so the user unit starts in every user session.
          Set services.qmtcode.user to run it for one account and enable lingering.
        ''
        ++ lib.optional (cfg.openFirewall && firewallPorts == []) ''
          services.qmtcode.openFirewall is set, but no public TCP port could be derived.
          Localhost binds and mesh port 0 are not opened. Iroh still uses UDP, which this option does not open.
        '';

      assertions = [
        {
          assertion = let
            user = config.users.users.${cfg.user};
          in
            cfg.user
            == null
            || user.isNormalUser
            || (user.home != null && user.home != "/var/empty");
          message = "services.qmtcode.user must name a declared users.users entry with a home directory.";
        }
      ];
    })
    (lib.mkIf (nixos && cfg.user != null) {
      users.users.${cfg.user}.linger = lib.mkDefault true;
      systemd.user.services.qmtcode.unitConfig.ConditionUser = cfg.user;
    })
    (lib.mkIf (nixos && cfg.openFirewall && firewallPorts != []) {
      networking.firewall.allowedTCPPorts = firewallPorts;
    })
  ]);
}
