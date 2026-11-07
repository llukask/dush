# dush

Faster `du -sh *` using [diskus](https://crates.io/crates/diskus).

## Usage

```text
$ dush ~/dev
  3.4 GiB  ████████████████░░░░░░░░  68.1%  dev/
  1.2 GiB  ████████░░░░░░░░░░░░░░░░  24.3%  docs/
280.0 MiB  █░░░░░░░░░░░░░░░░░░░░░░░   5.5%  scratch/
───────────
  4.8 GiB  total
```

Common flags:

```text
dush                  # current directory
dush ~/dev            # specific path
dush -n 20 ~/dev      # top 20 entries instead of top 10
dush -A ~/dev         # all entries
dush -L 2 ~/dev       # drill two levels deep
dush -a ~/dev         # apparent size, like `du --apparent-size`
dush -f json ~/dev    # machine-readable output (csv, tsv, json, text)
```

`dush-gpui` is a [gpui](https://gpui.rs) GUI that shows the same data as
a tree you can explore with the mouse or the keyboard: arrows or `hjkl`
to move, expand and collapse, `enter` to toggle, `q` to quit. Directory
sizes fill in while you browse. It needs the `gpui` feature and a few
system libraries, see below.

```text
cargo run --release --features gpui --bin dush-gpui -- ~/dev
cargo run --release --features gpui --bin dush-gpui -- -a ~/dev   # apparent size
nix run .#dush-gpui -- ~/dev
```

## Performance

On my machine `du -sh /*` runs in ~4m1s, `dush /` runs in ~16s.

## Install

```text
cargo install --path .              # CLI only
cargo install --path . --features gpui  # CLI + GUI
```

## Requirements

- Rust **1.85+** (edition 2024).
- Linux, macOS, or any Unix where `diskus` works. The CLI is pure Rust
  and has no system dependencies.
- The GUI binary needs `libxkbcommon` and `libxcb` (plus `pkg-config`)
  to build, and a Vulkan driver plus Wayland or X11 at runtime. On
  NixOS, `nix develop` in this repo drops you into a shell that has
  them all.

## License

MIT - see [LICENSE](LICENSE)
