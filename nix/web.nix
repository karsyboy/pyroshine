# The web management interface's static files (pyroshine-ui/). Node.js is
# needed only to build them; the server serves them from
# share/pyroshine/web next to its executable. importNpmLock reads the
# committed package-lock.json, so there is no separate dependency hash.
{
  lib,
  buildNpmPackage,
  importNpmLock,
}:

let
  root = ../pyroshine-ui;
in
buildNpmPackage {
  pname = "pyroshine-web";
  version = (lib.importJSON (root + "/package.json")).version;

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.difference root (
      lib.fileset.unions [
        (lib.fileset.maybeMissing (root + "/node_modules"))
        (lib.fileset.maybeMissing (root + "/dist"))
      ]
    );
  };

  npmDeps = importNpmLock { npmRoot = root; };
  npmConfigHook = importNpmLock.npmConfigHook;

  installPhase = ''
    runHook preInstall
    cp -r dist $out
    runHook postInstall
  '';
}
