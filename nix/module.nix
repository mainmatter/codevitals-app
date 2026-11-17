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

  # Prometheus reads JSON as the YAML it is.
  alert = name: expr: for: summary: {
    alert = name;
    inherit expr for;
    labels.severity = "warning";
    annotations.summary = summary;
  };
  alerts.groups = [
    {
      name = "cv";
      rules = [
        (alert "CvDown" ''up{job="cv"} == 0'' "5m" "cv is not answering Prometheus.")
        (alert "CvRepoSyncFailing" "cv_repo_sync_failures_consecutive >= 3" "0m"
          "Repository {{ $labels.repository }} failed its last {{ $value }} syncs; see the journal."
        )
        (alert "CvRepoStale" "time() - cv_repo_last_sync_success_timestamp_seconds > 6 * 3600" "0m"
          "Repository {{ $labels.repository }} has not synced for over 6 hours."
        )
        (alert "CvSyncStuck"
          "cv_syncs_running > 0 and on () sum(increase(cv_sync_duration_seconds_count[3h])) == 0" "0m"
          "Syncs are running, but none has ended for 3 hours."
        )
        (alert "CvResyncFailing" "time() - cv_resync_last_success_timestamp_seconds > 3 * 3600" "0m"
          "The hourly resync has not scheduled every installation for 3 hours."
        )
        (alert "CvBacklog" "cv_syncs_pending > cv_syncs_running" "2h"
          "Syncs have been waiting for a turn for 2 hours."
        )
        (alert "CvWebhooksUnauthorized"
          ''increase(cv_webhook_deliveries_total{outcome="unauthorized"}[15m]) > 5'' "0m"
          "Webhook deliveries fail their signature check: a rotated secret, or someone probing."
        )
        (alert "CvWebhooksFailing"
          ''increase(cv_webhook_deliveries_total{outcome=~"unreadable|unavailable"}[1h]) > 0'' "0m"
          "Signed webhook deliveries were turned away."
        )
        (alert "CvGithubRefused" ''increase(cv_github_errors_total{status=~"403|429"}[1h]) > 0'' "0m"
          "GitHub refused requests, failing syncs: a rate limit, or a lost permission."
        )
        (alert "CvMemoryHigh" ''process_resident_memory_bytes{job="cv"} > 3e9'' "15m"
          "cv is past its MemoryHigh, and being throttled."
        )
      ];
    }
    {
      name = "cv-postgres";
      rules = [
        (alert "PostgresDown" ''pg_up{job="cv-postgres"} == 0'' "5m" "Postgres is not answering.")
        (alert "PostgresConnectionsHigh"
          "sum(pg_stat_activity_count) > 0.8 * max(pg_settings_max_connections)" "10m"
          "Postgres is past 80% of its connections."
        )
        (alert "PostgresLongTransaction"
          ''max(pg_stat_activity_max_tx_duration{state=~"active|idle in transaction"}) > 1800'' "0m"
          "A Postgres transaction has been open for over 30 minutes."
        )
        (alert "PostgresDeadlocks" "increase(pg_stat_database_deadlocks[1h]) > 0" "0m"
          "Postgres resolved a deadlock."
        )
      ];
    }
  ];
in
{
  options.services.cv = {
    enable = lib.mkEnableOption "the cv server";

    listen = mkOption {
      type = types.str;
      example = "127.0.0.1:3000";
      description = "Address to listen on.";
    };

    metricsListen = mkOption {
      type = types.str;
      default = "127.0.0.1:9464";
      description = "Address to serve Prometheus metrics on. Keep it off the public listener.";
    };

    otlpTracesUrl = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "http://127.0.0.1:4318/v1/traces";
      description = "OTLP/HTTP URL to send traces to, e.g. Alloy's; none are sent if null.";
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

    trackedBranches = mkOption {
      type = types.attrsOf (types.listOf types.str);
      default = { };
      example = {
        "123456789" = [ "release" ];
      };
      description = "Branches to track besides each repository's default branch, by repository ID.";
    };

    webhookSecretFile = mkOption {
      type = types.str;
      description = "File holding the app's webhook secret.";
    };
  };

  config = lib.mkIf cfg.enable {
    # The service signs in as the system user `cv`, over the socket, without a password. Analyses
    # read as members of the built-in `pg_read_all_data`, which reads every database here.
    services.postgresql = {
      enable = true;
      # The migrations rely on it. Conflicts with a host that picks another version on purpose.
      package = pkgs.postgresql_18;
      ensureDatabases = [ "cv" ];
      ensureUsers = [
        {
          name = "cv";
          ensureDBOwnership = true;
        }
      ];
      # Slow statements and lock waits, in the journal.
      settings = {
        log_min_duration_statement = lib.mkDefault "5s";
        log_lock_waits = lib.mkDefault true;
      };
    };

    # With a Prometheus on this host: scraped, and alerting on what goes wrong.
    services.prometheus = lib.mkIf config.services.prometheus.enable {
      exporters.postgres = {
        enable = true;
        listenAddress = "127.0.0.1";
        runAsLocalSuperUser = true;
      };
      scrapeConfigs = [
        {
          job_name = "cv";
          static_configs = [ { targets = [ cfg.metricsListen ]; } ];
        }
        {
          job_name = "cv-postgres";
          static_configs = [
            {
              targets = [ "127.0.0.1:${toString config.services.prometheus.exporters.postgres.port}" ];
            }
          ];
        }
      ];
      ruleFiles = [ (pkgs.writeText "cv-alerts.json" (builtins.toJSON alerts)) ];
    };

    systemd.services.cv = {
      description = "cv";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      requires = [ "postgresql.target" ];
      after = [
        "network-online.target"
        "postgresql.target"
      ];

      serviceConfig = {
        ExecStart = lib.escapeShellArgs (
          [
            (lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default)
            "--listen=${cfg.listen}"
            "--metrics-listen=${cfg.metricsListen}"
            "--log-format=json"
            "--database-url=postgres:///cv?host=/run/postgresql&user=cv"
            "--mirror-dir=%S/cv"
            "--app-id=${toString cfg.appId}"
            "--client-id=${cfg.clientId}"
            "--private-key-file=%d/private-key"
            "--client-secret-file=%d/client-secret"
            "--webhook-secret-file=%d/webhook-secret"
          ]
          ++ lib.optional (cfg.otlpTracesUrl != null) "--otlp-traces-url=${cfg.otlpTracesUrl}"
          ++ lib.concatLists (
            lib.mapAttrsToList (
              repository: map (branch: "--tracked-branch=${repository}:${branch}")
            ) cfg.trackedBranches
          )
        );
        # Copies the secrets somewhere only this service can read them, as `%d/<name>`.
        LoadCredential = [
          "private-key:${cfg.privateKeyFile}"
          "client-secret:${cfg.clientSecretFile}"
          "webhook-secret:${cfg.webhookSecretFile}"
        ];
        DynamicUser = true;
        StateDirectory = "cv";
        Restart = "on-failure";
        # Past the default 100ms, a crash loop would hit the start limit and give up quickly.
        RestartSec = "5s";
        # Stopping waits for git work in flight; a sync stopped anywhere is finished by the next.
        TimeoutStopSec = "30s";
        # Reads hold up to 4 syncs x 32 items x 16 MiB, webhooks 8 x 25 MiB, beside a fetch.
        MemoryHigh = lib.mkDefault "3G";
        MemoryMax = lib.mkDefault "4G";

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
