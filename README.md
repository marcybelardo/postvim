# postvim

A small, single-buffer UTF-8 terminal editor with a bounded subset of Vim-style
modal editing. See [the key reference](docs/keymaps.md) for supported commands.

## Development with Nix

With Nix installed and the `nix-command` and `flakes` experimental features enabled:

```sh
nix develop
cargo run
```

The development shell supplies Rust, Cargo, the C compiler/linker, rustfmt,
Clippy, rust-analyzer (with Rust sources), and Python 3 for the terminal tests.
No global compiler installation is needed. `flake.lock` pins Nixpkgs and the
Rust toolchain it provides; commit it alongside `flake.nix`. To update that pin,
run `nix flake update nixpkgs` and rerun verification.

Inside the shell:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
python3 tests/terminal_smoke.py target/debug/postvim
```

You can also run a single command with `nix develop -c cargo build`.
Without Nix, install Rust 1.88 or newer and a working C compiler/linker, then
use the same Cargo commands.

## Build, run, or install with Nix

From a checkout:

```sh
nix build
./result/bin/postvim
# Or build and run directly:
nix run . -- 'notes with spaces.txt'
# Build the release package and run its Rust tests:
nix flake check
# Install into your user profile:
nix profile install .#postvim
```

Once the flake files are published on GitHub, testers can use these without
cloning the repository:

```sh
nix run github:marcybelardo/postvim -- 'notes with spaces.txt'
nix profile install github:marcybelardo/postvim#postvim
```

For a particular release or commit, use
`github:marcybelardo/postvim/<tag-or-commit>#postvim` instead. These commands build
from source; they do not require a prebuilt release binary. The package uses
`Cargo.lock` for Rust dependencies and runs the Rust tests during the build.
The Python PTY tests are run separately in the development shell.

Another Nix flake can consume the package as an input:

```nix
inputs.postvim.url = "github:marcybelardo/postvim";

# In a NixOS module receiving the postvim input:
environment.systemPackages = [ postvim.packages.${pkgs.stdenv.hostPlatform.system}.postvim ];
# For Home Manager, use the same package in home.packages.
```

Pass the `postvim` input to the module (for example through `specialArgs` in
`nixosSystem`). Pin a tag or commit in the input URL when testing a release.

Outputs are provided for x86-64 Linux, ARM64 Linux, and ARM64 macOS; only
x86-64 Linux has been verified here. Current Nixpkgs no longer supports Intel
macOS. Editing requires an interactive terminal.
In a Git checkout, Nix only includes Git-tracked files: add new source files
before building. For a newly created, not-yet-staged flake, use `nix develop path:.`
or `nix build path:.` until its files have been added to Git.

## Philosophy

The modal text editor known as "vim" has existed since time immemorial. Its adherents stay loyal to its vision of simplicity, keyboard-focus, and extensibility, and in their united desire have made it what it is today. Vim, and its fork Neovim, are shining examples of open source development and what computers can be capable of.

But times change, and the old world cannot hold. As inheritors of its legacy, we respect where vim has brought us, but we must also keep looking forward at new horizons. Code, indeed text itself, is no longer confined to simple keystrokes. Physical and digital hands leave traces within computational systems, and we are finding new ways to do old work. Typing cannot live alone, but typing cannot die. Despite text no longer emanating only from our minds, we are ruled by it evermore. Vim cannot change; vim cannot stay the same.

Postvim is a reassertment of what makes vim great— its commands, simplicity, extensibility— and a challenge to preconceived notions of what vim _must_ be. We look back fondly, but we cannot give in to nostalgia. We will not assume compatibility, system-wise. We do not assert pre-included configuration and feature-completeness. We make the system adaptable, configurable, extensible, and it is up to each individual developer to make do of that what they will.