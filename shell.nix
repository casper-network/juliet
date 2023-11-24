let
  nixStable = builtins.fetchTarball {
    url = "https://github.com/nixos/nixpkgs/archive/23.05.tar.gz";
    sha256 = "10wn0l08j9lgqcw8177nh2ljrnxdrpri7bp0g7nvrsn9rkawvlbf";
  };
  rustOverlay = builtins.fetchTarball {
    url =
      "https://github.com/oxalica/rust-overlay/archive/e3ebc177291f5de627d6dfbac817b4a661b15d1c.tar.gz";
    sha256 = "1nc8a3fv4aqfj22dizvycr02bmj38qjndsjkkxr8v25dzwm1a4j5";
  };
in { pkgs ? import nixStable { overlays = [ (import rustOverlay) ]; } }:

let rustBin = pkgs.rust-bin.stable."1.70.0".default;
in pkgs.mkShell {
  nativeBuildInputs = [ (rustBin.override { extensions = [ "rust-src" ]; }) ];
}
