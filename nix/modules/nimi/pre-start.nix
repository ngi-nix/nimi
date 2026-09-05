{ lib, ... }:
let
  inherit (lib) mkOption types;
in
{
  _class = "nimi";

  options.services = mkOption {
    type = types.lazyAttrsOf (
      types.submoduleWith {
        modules = [
          {
            options.preStart = mkOption {
              description = ''
                Path to a executable to run before each start of this service.

                Runs before every start attempt, including restarts.
                If the script exits with a non-zero status, the service
                is considered failed and the restart policy applies.

                Set to `null` to disable.
              '';
              type = types.nullOr types.pathInStore;
              default = null;
              example = lib.literalExpression ''
                lib.getExe (
                  pkgs.writeShellApplication {
                    name = "example-pre-start";
                    text = '''
                      echo "preparing service..."
                    ''';
                  }
                )
              '';
            };
            options.readyCheck = mkOption {
              description = ''
                Path to an executable to run to determine if the service is ready.

                The executable should exit 0 when the service is ready.
                It will be polled repeatedly until it succeeds.
                Required for services that will be used as `afterReady` targets.

                Set to `null` to disable.
              '';
              type = types.nullOr types.pathInStore;
              default = null;
              example = lib.literalExpression ''
                lib.getExe (
                  pkgs.writeShellApplication {
                    name = "example-ready-check";
                    text = '''
                      curl -f http://localhost:8080/health || exit 1
                    ''';
                  }
                )
              '';
            };
          }
        ];
      }
    );
  };
}
