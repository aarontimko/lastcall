# Install

lastcall is a single binary. There is nothing to configure before it runs, and nothing is
written to the repositories it watches.

## From a release

Every release publishes four binaries and a `SHA256SUMS` file:

| file | for |
|---|---|
| `lastcall-<version>-aarch64-apple-darwin` | macOS on Apple silicon |
| `lastcall-<version>-x86_64-apple-darwin` | macOS on Intel |
| `lastcall-<version>-aarch64-unknown-linux-gnu` | Linux on arm64 |
| `lastcall-<version>-x86_64-unknown-linux-gnu` | Linux on x86_64 |

Pick the one for your machine, check it, and put it on your `PATH`:

```sh
version=0.2.0
target=aarch64-apple-darwin        # see the table above
base=https://github.com/aarontimko/lastcall/releases/download/v$version

curl -fsSLO "$base/lastcall-$version-$target"
curl -fsSLO "$base/SHA256SUMS"
shasum -a 256 --ignore-missing -c SHA256SUMS    # on Linux: sha256sum -c --ignore-missing SHA256SUMS
chmod +x "lastcall-$version-$target"
mv "lastcall-$version-$target" ~/.local/bin/lastcall    # any directory on your PATH
lastcall --version
```

The checksum step is not optional decoration: it is the only thing that tells you the file
you downloaded is the file that was built.

### Verify where the binary came from

Releases carry a GitHub build attestation, which says which workflow, commit and runner
produced the bytes. With the [GitHub CLI](https://cli.github.com):

```sh
gh attestation verify "lastcall-$version-$target" -R aarontimko/lastcall
```

### macOS: the first run

The binaries are not notarized, so Gatekeeper stops a file that carries the quarantine
attribute the first time it runs. A browser download carries it; a `curl` download does not,
and then the command below reports "No such xattr", which is fine. Either clear the
attribute:

```sh
xattr -d com.apple.quarantine "lastcall-$version-$target"
```

or open System Settings, Privacy and Security, and allow it there after the first refusal.

### Linux: which glibc

The Linux binaries are built on Ubuntu 22.04, so they need glibc 2.35 or newer. That covers
Ubuntu 22.04 and later, Debian 12 and later, Fedora 36 and later, and current rolling
distributions. On anything older, build from source. The x86_64 Linux binary is built on a
native runner and is smoke tested on an emulated x86_64 container, not on x86_64 hardware.

## From source

Requires [rustup](https://rustup.rs) and `git`. The repository pins its toolchain in
`rust-toolchain.toml`, and rustup installs it on first use.

```sh
cargo install --git https://github.com/aarontimko/lastcall --tag v0.2.0 lastcall
```

That puts `lastcall` in `~/.cargo/bin`. A clone plus `just cargo build --release -p lastcall`
works too, and is what you want if you intend to change the code:
[`CONTRIBUTING.md`](../CONTRIBUTING.md).

## Staying current

```sh
lastcall update --check    # is there a newer release?
lastcall update            # download it, verify its checksum, replace this binary
```

`lastcall update` refuses to touch a binary a package manager owns (Homebrew, `cargo
install`, nix): update it the way you installed it. Otherwise it downloads the asset for
your platform, checks it against the release's `SHA256SUMS`, and replaces the running
binary by an atomic rename, so a failed download or a wrong checksum leaves what you have
alone. It needs `curl` and write permission on the directory the binary sits in.

The terminal UI also asks once a day, in the background after the first screen is drawn,
whether a newer release exists, and shows `↑ <version>` in the header if one does. Click it
to read the whole sentence. To turn that off:

```toml
# ~/.config/lastcall/config.toml
[update]
check = false
```

## Uninstall

Delete the binary, and delete the state directory if you want the review ledger gone too:

```sh
rm "$(command -v lastcall)"
rm -r ~/.local/state/lastcall        # or $LASTCALL_STATE_DIR
rm -r ~/.config/lastcall             # if you wrote a config file
```

Nothing else is left behind: lastcall never writes inside the repositories it watches.
