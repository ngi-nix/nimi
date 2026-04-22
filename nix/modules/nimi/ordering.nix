{ lib, config, ... }:
let
  inherit (lib)
    mkOption
    types
    ;

  serviceNames = lib.attrNames config.services;
  orderingKeys = lib.attrNames config.ordering;
in
{
  _class = "nimi";

  options.ordering = mkOption {
    description = ''
      Service startup ordering constraints.

      Each attribute names a service and declares which other services
      it must wait for before starting. Services without ordering
      constraints (or not mentioned here) start immediately.

      This only controls startup order inside a single nimi instance.
      It applies equally to containers, NixOS, Home Manager, and
      local development runs.
    '';
    example = lib.literalExpression ''
      {
        backend.after  = [ "database" ];
        frontend.after = [ "database" "backend" ];
      }
    '';
    type = types.attrsOf (
      types.submodule {
        options.after = mkOption {
          description = ''
            List of service names that must have started before this
            service is spawned.
          '';
          type = types.listOf types.str;
          default = [ ];
        };
        options.afterReady = mkOption {
          description = ''
            List of service names that must be ready before this
            service is spawned. Each target must declare a
            readiness check.
          '';
          type = types.listOf types.str;
          default = [ ];
        };
      }
    );
    default = { };
  };

  config.assertions =
    let
      mkKeyAssertion = name: {
        assertion = lib.elem name serviceNames;
        message = "ordering.${name} references a service that does not exist.";
      };

      mkAfterAssertions =
        name: deps:
        map (dep: {
          assertion = lib.elem dep serviceNames;
          message = "ordering.${name}.after references unknown service \"${dep}\".";
        }) deps;

      mkAfterReadyAssertions =
        name: deps:
        map (dep: {
          assertion = lib.elem dep serviceNames;
          message = "ordering.${name}.afterReady references unknown service \"${dep}\".";
        }) deps
        ++ map (dep: {
          assertion = config.services.${dep}.readyCheck != null;
          message = "ordering.${name}.afterReady references service \"${dep}\" without readyCheck.";
        }) deps;
    in
    (map mkKeyAssertion orderingKeys)
    ++ (lib.concatLists (lib.mapAttrsToList (name: o: mkAfterAssertions name o.after) config.ordering))
    ++ (lib.concatLists (
      lib.mapAttrsToList (name: o: mkAfterReadyAssertions name o.afterReady) config.ordering
    ));
}
