{
  description = "dush dev shell — Rust toolchain + GTK4 system libs for the GUI binary";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in {
        devShells.default = pkgs.mkShell {
          # Build-time helpers used by `cargo build`: a Rust toolchain plus
          # the bindings/system-deps glue (`pkg-config`) that gtk4-rs uses
          # to discover the GTK system libraries below.
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            pkg-config
            wrapGAppsHook4
          ];

          # Runtime/link-time GTK stack. gtk4-rs (and its glib/gdk/cairo/
          # pango sister crates) all use pkg-config to find these. We list
          # them explicitly rather than relying on a meta-package so the
          # closure stays small and the breakage mode is obvious.
          buildInputs = with pkgs; [
            gtk4
            glib
            gdk-pixbuf
            graphene
            cairo
            pango
            librsvg
          ];
        };
      });
}
