{ lib, ... }:
let
  inherit (lib) mkOption types;
in
{
  _class = "nimi";

  options.settings.ready = mkOption {
    description = ''
      Readiness check behavior for the nimi process manager.

      This section controls how long to wait for a service's readiness check
      to pass before considering the service started. The readiness
      check runs in parallel with the service, so the service has
      time to initialize while we verify it's ready.
    '';
    example = lib.literalExpression ''
      {
        timeout = 30000;
      }
    '';
    type = types.submodule {
      options = {
        timeout = mkOption {
          description = ''
            Maximum time to wait for readiness check to pass (in milliseconds).

            If the readiness check doesn't succeed within this time, the
            service will fail to start. Set to a higher value if your service
            takes longer to initialize.
          '';
          type = types.ints.positive;
          default = 30000;
          example = lib.literalExpression "30000";
        };
      };
    };
    default = { };
  };
}

