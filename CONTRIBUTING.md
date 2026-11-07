# Contributing to GlassDock

Thanks for taking the time to improve GlassDock. Bug reports, shader tweaks, packaging and docs are
all welcome.

## Before you start

- For anything bigger than a small fix, open an issue first so we can agree on the approach.
- The dock is intentionally small: no toolkit, no config language, no daemon besides itself. Features
  that need a lot of new surface area may be better as a separate tool talking to `glassdockctl`.

## Development setup

You need a Rust toolchain (stable), the EGL and wayland-egl development files, and a Hyprland
session to try your changes in.

```bash
git clone https://github.com/OWNER/glassdock.git
cd glassdock
cargo build --release
cargo test
```

Run your build next to an installed one without disturbing it:

```bash
glassdockctl quit                                  # stop the installed dock (if any)
GLASSDOCK_BIN=target/release/glassdock scripts/glassdock-launch
```

Useful tools while hacking:

```bash
GLASSDOCK_DEBUG=1 target/release/glassdock         # verbose logging on stderr
glassdockctl dump | python3 -m json.tool           # internal state
glassdockctl reveal all                            # force the dock open for 4 s
glassdockctl hover eDP-1 300                       # fake the pointer at x=300
glassdockctl shader shaders/                       # hot-reload the glass shader
```

The glass shader can be edited live: change `shaders/glass.frag`, run the `shader` command again,
and the dock recompiles it without restarting. If it does not compile, the old shader stays.

## Code layout

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). In short: `dock.rs` is pure maths and is where
most behaviour changes belong (with a unit test); `model.rs` knows about the desktop; `gfx.rs` and
`shaders/` draw; `main.rs` glues Wayland events to all of that.

## Style

- `cargo fmt` before committing (settings in `rustfmt.toml`) and `cargo clippy -- -D warnings`
  should be clean.
- Comments explain *why*, in English. Keep user-visible strings in `src/i18n.rs`.
- No `unsafe` outside `gfx.rs` and the EGL and `wl_egl_window` glue in `main.rs`, and every `unsafe`
  block gets a comment saying why it is sound.
- No new dependencies without a reason in the PR description. The dependency list is short on
  purpose.
- Shell scripts must pass `shellcheck` and stay POSIX-friendly where reasonable (`bash` is fine, they
  declare it).

## Tests

```bash
cargo test
```

Layout, magnification, hit testing, the pinned-apps model, icon rasterisation and i18n are covered.
Tests must not touch the real `~/.config`, the Hyprland socket or the network; use the temp
directory helpers already in the test modules. Visual changes cannot be unit-tested, so please
attach a screenshot or a short recording to the PR.

## Pull request checklist

- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` pass
- [ ] New behaviour has a test, or the PR says why it cannot
- [ ] Docs and `CHANGELOG.md` (under *Unreleased*) are updated
- [ ] Visual change: screenshot or recording attached
- [ ] Tested on a real Hyprland session (say which version and GPU)

## Commit messages

Short imperative subject line (`Fix hot zone height on scaled outputs`), a blank line, then the
reason for the change if it is not obvious.

## Reporting security issues

Please do not open a public issue; see [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your work is released under the [MIT License](LICENSE).
