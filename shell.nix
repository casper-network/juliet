{ pkgs ? (import <nixpkgs>) {
  overlays = [
    (import (builtins.fetchTarball
      "https://github.com/oxalica/rust-overlay/archive/e3ebc177291f5de627d6dfbac817b4a661b15d1c.tar.gz"))
  ];
} }:

let rustBin = pkgs.rust-bin.stable."1.70.0".default;
in pkgs.mkShell {
  nativeBuildInputs = [ (rustBin.override { extensions = [ "rust-src" ]; }) ];
}
