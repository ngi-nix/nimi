{ lib, config, ... }:
let
  inherit (lib) mkOption types;

  serviceNames = builtins.attrNames config.services;

  referencedDeps = lib.pipe config.ordering [
    builtins.attrValues
    (map (o: o.after))
    lib.flatten
  ];

  orderingKeys = builtins.attrNames config.ordering;
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
    type = types.attrsOf (types.submodule {
      options.after = mkOption {
        description = ''
          List of service names that must have started before this
          service is spawned.
        '';
        type = types.listOf types.str;
        default = [ ];
      };
    });
    default = { };
  };

  config.assertions =
    let
      mkKeyAssertion = name: {
        assertion = builtins.elem name serviceNames;
        message = "ordering.${name} references a service that does not exist.";
      };

      mkDepAssertions = name: deps:
        map (dep: {
          assertion = builtins.elem dep serviceNames;
          message = "ordering.${name}.after references unknown service \"${dep}\".";
        }) deps;
    in
    (map mkKeyAssertion orderingKeys)
    ++ (lib.concatLists (lib.mapAttrsToList (name: o: mkDepAssertions name o.after) config.ordering));
}
