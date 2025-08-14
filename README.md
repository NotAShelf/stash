# Stash

Wayland clipboard "manager" with fast persistent history and multi-media
support. Stores and previews clipboard entries (text, images) on the command
line.

## Installation

### With Nix

Nix is the recommended way of downloading Stash. You can install it using Nix
flakes using `nix profile add` if on non-nixos or add Stash as a flake input if
you are on NixOS.

```nix
{
  # Add Stash to your inputs like so
  inputs.stash.url = "github:notashelf/stash";

  outputs = { /* ... */ };
}
```

Then you can get the package from your flake input, and add it to your packages
to make `stash` available in your system.

```nix
{inputs, pkgs, ...}: let
  stashPkg = inputs.stash.packages.${pkgs.stdenv.hostPlatform}.stash;
in {
  environment.systemPackages = [stashPkg];

  # Additionally feel free to add the Stash package in `systemd.packages` to
  # automatically run the Stash watch daemon, which will watch your primary
  # clipboard for changes and persist them.
  systemd.packages = [stashPkg];
}
```

You can also run it one time with `nix run`

```sh
nix run github:notashelf/stash -- watch # start the watch daemon
```

### Without Nix

[GitHub Releases]: https://github.com/notashelf/stash/releases

You can also install Stash on any of your systems _without_ using Nix. New
releases are made when a version gets tagged, and are available under
[GitHub Releases]. To install Stash on your system without Nix, eiter:

- Download a tagged release from [GitHub Releases] for your platform and place
  the binary in your `$PATH`.
- Build from source with Rust:

  ```bash
  cargo install --git https://github.com/notashelf/stash
  ```

## Features

- Stores clipboard entries with automatic MIME detection
- Fast persistent storage using SQLite
- List, search, decode, delete, and wipe clipboard history
- Backwards compatible with Cliphist TSV format
  - Import clipboard history from TSV (e.g., from `cliphist list`)
- Image preview (shows dimensions and format)
- Deduplication and entry limit control
- Text previews with customizable width

## Usage

Command interface is only slightly different from Cliphist. In most cases, it
will be as simple as replacing `cliphist` with `stash` in your commands, aliases
or scripts.

### Store an entry

```bash
echo "some clipboard text" | stash store
```

### List entries

```bash
stash list
```

### Decode an entry by ID

```bash
stash decode --input "1234"
```

### Delete entries matching a query

```bash
stash delete --type query --arg "some text"
```

### Delete multiple entries by ID (from a file or stdin)

```bash
stash delete --type id < ids.txt
```

### Wipe all entries

```bash
stash wipe
```

### Watch clipboard for changes and store automatically

```bash
stash watch
```

This runs a daemon that monitors the clipboard and stores new entries
automatically.

### Options

Some commands take additional flags to modify Stash's behavior. See each
commands `--help` text for more details. The following are generally standard:

- `--db-path <path>`: Custom database path
- `--max-items <N>`: Maximum number of entries to keep (oldest trimmed)
- `--max-dedupe-search <N>`: Deduplication window size
- `--preview-width <N>`: Text preview max width for `list`
- `--version`: Print the current version and exit

## Tips & Tricks

### Migrating from Cliphist

Stash is designed to be a drop-in replacement for Cliphist, with only minor
improvements. If you are migrating from Cliphist, here are a few things you
should know.

- Most Cliphist commands have direct equivalents in Stash. For example,
  `cliphist store` -> `stash store`, `cliphist list` -> `stash list`, etc.
- Cliphist uses `delete-query`; in Stash, you must use
  `stash delete --type query --arg "your query"`.
- Both Cliphist and Stash support deleting by ID, including from stdin or a
  file.
- Stash respects the `STASH_CLIPBOARD_STATE` environment variable for
  sensitive/clear entries, just like Cliphist. The `STASH_` prefix is added for
  granularity, you must update your scripts.
- You can export your Cliphist history to TSV and import it into Stash (see
  below).
- Stash supports text and image previews, including dimensions and format.
- Stash adds a `watch` command to automatically store clipboard changes. This is
  an alternative to `wl-paste --watch cliphist list`. You can avoid shelling out
  and depending on `wl-paste` as Stash implements it through `wl-clipboard-rs`
  crate.

### TSV Export and Import

Both Stash and Cliphist support TSV format for clipboard history. You can export
from Cliphist and import into Stash, or use Stash to export TSV for
interoperability.

**Export TSV from Cliphist:**

```bash
cliphist list --db ~/.cache/cliphist/db > cliphist.tsv
```

**Import TSV into Stash:**

```bash
stash --import < cliphist.tsv
```

**Export TSV from Stash:**

```bash
stash list > stash.tsv
```

**Import TSV into Cliphist:**

```bash
cliphist --import < stash.tsv
```

### More Tricks

- Use `stash list` to export your clipboard history in TSV format. This displays
  your clipboard in the same format as `cliphist list`
- Use `stash import --type tsv` to import TSV clipboard history from Cliphist or
  other tools.
