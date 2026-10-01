# NixOS module: services.smtp-to-telegram.
#
# Secrets (bot token, SMTP credentials, optionally chat ids) are passed as
# paths outside the Nix store and handed to the service with systemd
# LoadCredential=, so only the service can read them and they never appear
# on the command line.
self:
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:

let
  cfg = config.services.smtp-to-telegram;
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    optional
    optionals
    types
    ;

  secretPath = types.strMatching "/.*" // {
    description = "absolute path outside the Nix store";
  };

  args = [
    "--log-level"
    cfg.logLevel
    "--smtp-max-envelope-size"
    cfg.maxMessageSize
    "--telegram-api-parse-mode"
    cfg.parseMode
  ]
  ++ lib.concatMap (address: [
    "--smtp-listen"
    address
  ]) cfg.listen
  ++ lib.concatMap (network: [
    "--plaintext-auth-networks"
    network
  ]) cfg.plaintextAuthNetworks
  ++ optionals (cfg.hostname != null) [
    "--smtp-primary-host"
    cfg.hostname
  ]
  ++ optional cfg.allowAnonymous "--allow-anonymous"
  ++ optionals (cfg.chatIds != [ ]) [
    "--telegram-chat-ids"
    (lib.concatStringsSep "," cfg.chatIds)
  ]
  ++ optionals (cfg.messageTemplate != null) [
    "--message-template-file"
    "${pkgs.writeText "smtp-to-telegram-template" cfg.messageTemplate}"
  ]
  ++ cfg.extraArgs;

  # `%d` is systemd's credentials directory; these must stay unescaped.
  credentialArgs = [
    "--telegram-bot-token-file %d/telegram-bot-token"
  ]
  ++ optional (cfg.credentialsFile != null) "--credentials-file %d/smtp-credentials"
  ++ optional (cfg.chatIdsFile != null) "--telegram-chat-ids-file %d/telegram-chat-ids";
in
{
  options.services.smtp-to-telegram = {
    enable = mkEnableOption "smtp_to_telegram, an SMTP server that forwards every email to Telegram";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "smtp-to-telegram.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The smtp_to_telegram package to run.";
    };

    listen = mkOption {
      type = types.nonEmptyListOf types.str;
      default = [ "127.0.0.1:2525" ];
      example = [
        "0.0.0.0:25"
        "[::]:25"
      ];
      description = ''
        Addresses to accept SMTP on. Ports below 1024 work: the service has
        CAP_NET_BIND_SERVICE. A specific address must exist when the service
        starts; to accept on some interfaces only, listening on the wildcard
        address and filtering with the firewall is the robust choice.
      '';
    };

    hostname = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Host name in the SMTP greeting; the system host name when null.";
    };

    maxMessageSize = mkOption {
      type = types.str;
      default = "50m";
      description = "Largest accepted message, e.g. `10m`, `50MB`, `4MiB`.";
    };

    allowAnonymous = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Accept mail from clients that did not authenticate. With
        `credentialsFile` also set, both authenticated and anonymous
        delivery are accepted.
      '';
    };

    credentialsFile = mkOption {
      type = types.nullOr secretPath;
      default = null;
      example = "/run/agenix/smtp-to-telegram-credentials";
      description = ''
        File with one `username:password` per line, accepted by SMTP
        AUTH PLAIN and LOGIN. Read through LoadCredential=.
      '';
    };

    plaintextAuthNetworks = mkOption {
      type = types.listOf types.str;
      default = [
        "127.0.0.0/8"
        "::1/128"
      ];
      example = [ "10.145.0.0/24" ];
      description = ''
        Client networks to which AUTH is offered. There is no TLS, so the
        password crosses the network in the clear: list only networks that
        are trusted or already encrypted (VPN tunnels). Clients elsewhere
        are not offered AUTH and get 538 if they try.
      '';
    };

    botTokenFile = mkOption {
      type = secretPath;
      example = "/run/agenix/telegram-bot-token";
      description = "File containing the Telegram bot token. Read through LoadCredential=.";
    };

    chatIds = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "123456789"
        "-1001234567890"
      ];
      description = "Telegram chats to deliver to. Set this or `chatIdsFile`.";
    };

    chatIdsFile = mkOption {
      type = types.nullOr secretPath;
      default = null;
      description = ''
        File with the chat ids, separated by commas or whitespace, for when
        they should not be in the Nix store. Read through LoadCredential=.
      '';
    };

    parseMode = mkOption {
      type = types.enum [
        "none"
        "MarkdownV2"
        "HTML"
        "Markdown"
      ];
      default = "none";
      description = ''
        Telegram parse_mode for the message. Values substituted into the
        template are escaped for it; the template itself is markup.
      '';
    };

    messageTemplate = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "*{subject}*\\n{from} → {to}\\n\\n{body}\\n\\n{attachments_details}";
      description = ''
        Message template with the placeholders {from}, {to}, {subject},
        {body} and {attachments_details}; `\n` (backslash, n) is a newline,
        as is a real newline. Null keeps the built-in default.
      '';
    };

    logLevel = mkOption {
      type = types.enum [
        "error"
        "warn"
        "info"
        "debug"
        "trace"
      ];
      default = "info";
      description = "Log level.";
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "--forwarded-attachment-max-size"
        "0"
        "--telegram-api-extra-param"
        "message_thread_id=42"
      ];
      description = "Further command line arguments; see `smtp_to_telegram --help`. Never put secrets here.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = (cfg.chatIds != [ ]) != (cfg.chatIdsFile != null);
        message = "services.smtp-to-telegram: set exactly one of chatIds and chatIdsFile.";
      }
      {
        assertion = cfg.allowAnonymous || cfg.credentialsFile != null;
        message = "services.smtp-to-telegram: set credentialsFile or allowAnonymous, otherwise nobody can deliver mail.";
      }
    ];

    systemd.services.smtp-to-telegram = {
      description = "SMTP to Telegram relay";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];

      serviceConfig = {
        ExecStart = lib.concatStringsSep " " (
          [
            (lib.getExe cfg.package)
            (utils.escapeSystemdExecArgs args)
          ]
          ++ credentialArgs
        );
        LoadCredential = [
          "telegram-bot-token:${cfg.botTokenFile}"
        ]
        ++ optional (cfg.credentialsFile != null) "smtp-credentials:${cfg.credentialsFile}"
        ++ optional (cfg.chatIdsFile != null) "telegram-chat-ids:${cfg.chatIdsFile}";
        Restart = "on-failure";
        RestartSec = "5s";

        # A throwaway user; the only privilege is binding ports below 1024.
        DynamicUser = true;
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        NoNewPrivileges = true;

        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        DevicePolicy = "closed";
        ProtectProc = "invisible";
        ProcSubset = "pid";
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RemoveIPC = true;
        UMask = "0077";
        # AF_UNIX: glibc reaches nscd for host name lookups.
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
      };
    };
  };
}
