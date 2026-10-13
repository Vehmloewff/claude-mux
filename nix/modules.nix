# NixOS and Home Manager modules that run claudexd as a systemd user service.
{ self }:
let
  options =
    { lib, pkgs }:
    {
      enable = lib.mkEnableOption "the claudexd daemon (systemd user service)";

      package = lib.mkOption {
        type = lib.types.package;
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        defaultText = lib.literalExpression "claudex.packages.\${system}.default";
        description = "The claudex package (provides `claudex` and `claudexd`).";
      };

      extraPackages = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = [ ];
        example = lib.literalExpression "[ pkgs.claude-code pkgs.git pkgs.nodejs ]";
        description = ''
          Packages put on PATH for claudexd and the claude sessions it runs,
          ahead of the user's profile. Add `claude-code` here unless it is
          already in the user's profile.
        '';
      };

      environment = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = { };
        example = {
          CLAUDEX_CLAUDE_BIN = "/opt/claude/bin/claude";
        };
        description = "Extra environment variables for claudexd (and its sessions).";
      };
    };

  # PATH for the service: extra packages, claudex itself, then the usual
  # per-user and system profiles so a user-installed claude is found.
  # %h / %u are expanded by systemd.
  servicePath =
    { lib, cfg }:
    lib.concatStringsSep ":" [
      (lib.makeBinPath ([ cfg.package ] ++ cfg.extraPackages))
      "/run/wrappers/bin"
      "%h/.nix-profile/bin"
      "%h/.local/state/nix/profile/bin"
      "/etc/profiles/per-user/%u/bin"
      "/nix/var/nix/profiles/default/bin"
      "/run/current-system/sw/bin"
      "/usr/local/bin"
      "/usr/bin"
      "/bin"
    ];

  serviceEnvironment =
    { lib, cfg }:
    {
      PATH = servicePath { inherit lib cfg; };
    }
    // cfg.environment;

  serviceConfig = cfg: {
    ExecStart = "${cfg.package}/bin/claudexd";
    Restart = "on-failure";
    RestartSec = 2;
    # Only claudexd gets SIGTERM; it stops its claude sessions itself and
    # records them as resumable before exiting.
    KillMode = "mixed";
    TimeoutStopSec = 15;
  };

  description = "claudex daemon (Claude Code session manager)";
in
{
  nixos =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      cfg = config.services.claudexd;
    in
    {
      options.services.claudexd = options { inherit lib pkgs; };

      config = lib.mkIf cfg.enable {
        environment.systemPackages = [ cfg.package ];

        systemd.user.services.claudexd = {
          inherit description;
          wantedBy = [ "default.target" ];
          # Replaces NixOS's default unit PATH; the system profile on our PATH
          # already provides coreutils and friends.
          environment = lib.mapAttrs (_: lib.mkForce) (serviceEnvironment {
            inherit lib cfg;
          });
          serviceConfig = serviceConfig cfg;
        };
      };
    };

  home-manager =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      cfg = config.services.claudexd;
    in
    {
      options.services.claudexd = options { inherit lib pkgs; };

      config = lib.mkIf cfg.enable {
        assertions = [
          (lib.hm.assertions.assertPlatform "services.claudexd" pkgs lib.platforms.linux)
        ];

        home.packages = [ cfg.package ];

        systemd.user.services.claudexd = {
          Unit.Description = description;
          Service = serviceConfig cfg // {
            Environment = lib.mapAttrsToList (name: value: "${name}=${value}") (serviceEnvironment {
              inherit lib cfg;
            });
          };
          Install.WantedBy = [ "default.target" ];
        };
      };
    };
}
