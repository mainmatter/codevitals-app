# Runs the `cv` from the same flake: `nix flake check` proves this file passes exactly the flags it
# takes, so it can't be swapped for another version.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.cv;
  inherit (lib) mkOption types;
in
{
  options.services.cv = {
    enable = lib.mkEnableOption "the cv server";

    listen = mkOption {
      type = types.str;
      example = "127.0.0.1:3000";
      description = "Address to listen on.";
    };

    appId = mkOption {
      type = types.ints.unsigned;
      description = "The app's ID.";
    };

    clientId = mkOption {
      type = types.str;
      description = "The app's client ID.";
    };

    # Secrets are passed as paths to files outside the Nix store, which is world-readable. They are
    # strings rather than `types.path`, so a path literal such as `./key.pem` is refused instead of
    # copied into the store.
    privateKeyFile = mkOption {
      type = types.str;
      description = "File holding the app's private key, PEM-encoded.";
    };

    clientSecretFile = mkOption {
      type = types.str;
      description = "File holding the app's client secret.";
    };

    webhookSecretFile = mkOption {
      type = types.str;
      description = "File holding the app's webhook secret.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.cv = {
      description = "cv";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];

      serviceConfig = {
        ExecStart = lib.escapeShellArgs [
          (lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default)
          "--listen=${cfg.listen}"
          "--app-id=${toString cfg.appId}"
          "--client-id=${cfg.clientId}"
          "--private-key-file=%d/private-key"
          "--client-secret-file=%d/client-secret"
          "--webhook-secret-file=%d/webhook-secret"
        ];
        # Copies the secrets somewhere only this service can read them, as `%d/<name>`.
        LoadCredential = [
          "private-key:${cfg.privateKeyFile}"
          "client-secret:${cfg.clientSecretFile}"
          "webhook-secret:${cfg.webhookSecretFile}"
        ];
        DynamicUser = true;
        Restart = "on-failure";

        CapabilityBoundingSet = "";
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateUsers = true;
        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectProc = "invisible";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
        UMask = "0077";
      };
    };
  };
}
