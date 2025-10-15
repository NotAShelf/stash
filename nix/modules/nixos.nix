self: {
  config,
  lib,
  pkgs,
  ...
}: let
  inherit (lib.modules) mkIf;
  inherit (lib.options) mkOption mkEnableOption mkPackageOption literalMD;
  inherit (lib.types) listOf str;
  inherit (lib.strings) concatStringsSep;
  inherit (lib.meta) getExe;

  cfg = config.services.stash-clipboard;
in {
  options.services.stash-clipboard = {
    enable = mkEnableOption "stash, a Wayland clipboard manager";

    package = mkPackageOption self.packages.${pkgs.system} ["stash"] {};

    flags = mkOption {
      type = listOf str;
      default = [];
      example = ["--max-items 10"];
      description = "Flags to pass to stash watch.";
    };

    filterFile = mkOption {
      type = str;
      default = "";
      example = "{file}`/etc/stash/clipboard_filter`";
      description = literalMD ''
        File containing a regular expression to catch sensitive patterns. The file
        passed to this option must contain your regex pattern with no quotes.

        ::: {.tip}
        Example regex to block common password patterns:

        * `(password|secret|api[_-]?key|token)[=: ]+[^\s]+`
        :::
      '';
    };

    excludedApps = mkOption {
      type = listOf str;
      default = [];
      example = ["Bitwarden"];
      description = ''
        Stash will avoid storing data if the active window class matches the
        entries passed to this option. This is useful for avoiding persistent
        passwords in the database, while still allowing one-time copies.

        Entries from these apps are still copied to the clipboard, but it will
        never be put inside the database.
      '';
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
    systemd = {
      packages = [cfg.package];
      user.services.stash-clipboard = {
        description = "Stash clipboard manager daemon";
        wantedBy = ["graphical-session.target"];
        after = ["graphical-session.target"];

        serviceConfig = {
          ExecStart = "${getExe cfg.package} ${concatStringsSep " " cfg.flags} watch";
          LoadCredential = mkIf (cfg.filterFile != "") "clipboard_filter:${cfg.filterFile}";
        };

        environment = mkIf (cfg.excludedApps != []) {
          STASH_EXCLUDED_APPS = concatStringsSep "," cfg.excludedApps;
        };
      };
    };
  };
}
