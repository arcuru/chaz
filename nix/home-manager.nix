{
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.services.headjack;
  yamlFormat = pkgs.formats.yaml {};
in {
  options.services.headjack = {
    enable = mkEnableOption "headjack service";
    package = mkOption {
      type = types.package;
      default = pkgs.headjack;
      example = literalExample "pkgs.headjack";
      description = "Package for the headjack service.";
    };
    settings = mkOption {
      type = yamlFormat.type;
      default = {};
      example = literalExpression ''
        {
            homeserver_url = "https://matrix.org";
            username = "headjack";
            password = "hunter2";
            allow_list = "@me:matrix.org|@myfriend:matrix.org";
        }
      '';
      description = ''
        Configuration file for headjack. See the headjack documentation for more info.
      '';
    };
  };
  config = mkIf cfg.enable {
    systemd.user.services.headjack = {
      Unit = {
        Description = "Headjack Service";
        After = ["network-online.target"];
      };

      Service = {
        ExecStart = "${cfg.package}/bin/headjack --config ${yamlFormat.generate "config.yml" (cfg.settings)}";
        Restart = "always";
      };

      Install.WantedBy = ["default.target"];
    };
  };
}
