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

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        # System libraries needed by the gtk4-rs stack at link/runtime.
        # Shared between the GUI package and the dev shell so the two
        # don't drift.
        gtkLibs = with pkgs; [
          gtk4
          glib
          gdk-pixbuf
          graphene
          cairo
          pango
          librsvg
        ];

        # Libraries the gpui stack needs. `libxkbcommon` and `libxcb`
        # are linked at build time; the rest are dlopened at runtime
        # (Wayland and Vulkan by the blade renderer, fontconfig by
        # font-kit), so they also have to end up on the binary's rpath
        # or on LD_LIBRARY_PATH in the dev shell.
        gpuiLibs = with pkgs; [
          libxkbcommon
          libxcb
          libx11
          wayland
          vulkan-loader
          fontconfig
          freetype
        ];

        # CLI build: pure Rust, no system libs required. We disable
        # default features explicitly to be defensive in case `gui` ever
        # becomes default.
        dush = pkgs.rustPlatform.buildRustPackage {
          pname = "dush";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildNoDefaultFeatures = true;
          # Only build the CLI bin — the GUI bin has `required-features
          # = ["gui"]` and would be skipped anyway, but being explicit
          # keeps the build narrow and the closure small.
          cargoBuildFlags = [ "--bin" "dush" ];
          # The CLI has no integration tests that need GTK; run the
          # default test set.
          doCheck = true;
          meta = {
            description = cargoToml.package.description;
            license = pkgs.lib.licenses.mit;
            mainProgram = "dush";
          };
        };

        # GUI build: same crate, `gui` feature on, plus the GTK system
        # libraries and `wrapGAppsHook4` so the resulting binary can
        # find icon themes / GSettings schemas at runtime.
        dush-gui = pkgs.rustPlatform.buildRustPackage {
          pname = "dush-gui";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildFeatures = [ "gui" ];
          cargoBuildFlags = [ "--bin" "dush-gui" ];
          nativeBuildInputs = with pkgs; [ pkg-config wrapGAppsHook4 ];
          buildInputs = gtkLibs;
          # `cargo test` would also try to build the CLI test set; keep
          # it on so we don't silently regress the library.
          doCheck = true;
          meta = {
            description = cargoToml.package.description + " (GTK4 GUI)";
            license = pkgs.lib.licenses.mit;
            mainProgram = "dush-gui";
          };
        };

        # gpui GUI build. No wrapper hook is needed, but the dlopened
        # libraries in `gpuiLibs` are invisible to the linker, so the
        # fixup phase's rpath shrinking would drop them. We add them
        # back afterwards in `postFixup`.
        dush-gpui = pkgs.rustPlatform.buildRustPackage {
          pname = "dush-gpui";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildFeatures = [ "gpui" ];
          cargoBuildFlags = [ "--bin" "dush-gpui" ];
          nativeBuildInputs = with pkgs; [ pkg-config ];
          buildInputs = gpuiLibs;
          doCheck = true;
          postFixup = ''
            patchelf --add-rpath ${pkgs.lib.makeLibraryPath gpuiLibs} $out/bin/dush-gpui
          '';
          meta = {
            description = cargoToml.package.description + " (gpui GUI)";
            license = pkgs.lib.licenses.mit;
            mainProgram = "dush-gpui";
          };
        };
      in {
        packages = {
          inherit dush dush-gui dush-gpui;
          default = dush;
        };

        # `nix run` resolves to `apps.default`; declaring it explicitly
        # (rather than relying on the package fallback) makes the entry
        # point unambiguous if the package ever gains extra binaries.
        apps = {
          default = {
            type = "app";
            program = "${dush}/bin/dush";
          };
          dush = {
            type = "app";
            program = "${dush}/bin/dush";
          };
          dush-gui = {
            type = "app";
            program = "${dush-gui}/bin/dush-gui";
          };
          dush-gpui = {
            type = "app";
            program = "${dush-gpui}/bin/dush-gpui";
          };
        };

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

          # Runtime/link-time GTK stack — same set the GUI package
          # links against, kept in `gtkLibs` above so the two don't
          # drift. gtk4-rs (and its glib/gdk/cairo/pango sister crates)
          # all use pkg-config to find these.
          buildInputs = gtkLibs ++ gpuiLibs;

          # `cargo run --bin dush-gpui` has no rpath for the dlopened
          # gpui libraries, so expose them the old-fashioned way.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath gpuiLibs;
        };
      });
}
