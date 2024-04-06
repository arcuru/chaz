{
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.services.chaz;
  yamlFormat = pkgs.formats.yaml {};
in {
  options.services.chaz = {
    enable = mkEnableOption "chaz service";
    package = mkOption {
      type = types.package;
      default = pkgs.chaz;
      example = literalExample "pkgs.chaz";
      description = "Package for the chaz service.";
    };
    settings = mkOption {
      type = yamlFormat.type;
      default = {};
      example = literalExpression ''
        {
            homeserver_url = "https://matrix.jackson.dev";
            username = "chaz";
            password = "hunter2";
            allow_list = "@me:matrix.org|@myfriend:matrix.org";
        }
      '';
      description = ''
        Configuration file for chaz. See the chaz documentation for more info.
      '';
    };
  };
  config = mkIf cfg.enable {
    systemd.user.services.chaz = {
      Unit = {
        Description = "chaz Service";
        After = ["network-online.target"];
      };

      Service = {
        ExecStart = "${cfg.package}/bin/chaz --config ${yamlFormat.generate "config.yml" (cfg.settings)}";
        Restart = "always";
      };

      Install.WantedBy = ["default.target"];
    };
  };
}
