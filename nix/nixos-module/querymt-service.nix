{self}: {config, lib, pkgs, ...}: let
  cfg = config.services.querymt-service;
  bindAddr = "${cfg.listenAddress}:${toString cfg.port}";
in {
  options.services.querymt-service = {
    enable = lib.mkEnableOption "querymt-service (OpenAI-compatible QueryMT HTTP service)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.qmt-service;
      defaultText = "inputs.querymt.packages.${pkgs.system}.qmt-service";
      description = "The qmt-service package to run.";
    };

    listenAddress = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = "Address to bind qmt-service to.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8080;
      description = "Port to bind qmt-service to.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the configured TCP port in the firewall.";
    };

    providersFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/etc/querymt/providers.toml";
      description = "Optional providers config path passed via QMT_SERVICE_PROVIDERS.";
    };

    authKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/run/secrets/querymt-service-auth-key";
      description = "Optional path to bearer auth key file (QMT_SERVICE_AUTH_KEY_FILE).";
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = {};
      description = "Extra environment variables passed to qmt-service.";
    };

    extraArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      description = "Additional CLI args passed to qmt-service.";
    };
  };

  config = lib.mkIf cfg.enable {
    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [cfg.port];

    systemd.services.querymt-service = {
      description = "QueryMT OpenAI-compatible HTTP service";
      after = ["network-online.target"];
      wants = ["network-online.target"];
      wantedBy = ["multi-user.target"];

      environment =
        cfg.environment
        // {
          QMT_SERVICE_ADDR = bindAddr;
          HOME = "/var/lib/querymt-service";
          XDG_CACHE_HOME = "/var/cache/querymt-service";
        }
        // lib.optionalAttrs (cfg.providersFile != null) {
          QMT_SERVICE_PROVIDERS = cfg.providersFile;
        }
        // lib.optionalAttrs (cfg.authKeyFile != null) {
          QMT_SERVICE_AUTH_KEY_FILE = cfg.authKeyFile;
        };

      serviceConfig = {
        Type = "simple";
        DynamicUser = true;
        StateDirectory = "querymt-service";
        CacheDirectory = "querymt-service";
        WorkingDirectory = "%S/querymt-service";

        ExecStart =
          "${cfg.package}/bin/qmt-service ${lib.escapeShellArgs cfg.extraArgs}";

        Restart = "on-failure";
        RestartSec = 1;

        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime = true;

        RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX"];
        SystemCallFilter = ["@system-service" "~@privileged" "~@resources"];
        UMask = "0077";
      };
    };
  };
}
