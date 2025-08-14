# Stash

Wayland clipboard "manager" with fast persistent history and multi-media
support. Stores and previews clipboard entries (text, images) on the command
line.

## Features

- Stores clipboard entries with automatic MIME detection
- Fast persistent storage using SQLite
- List, search, decode, delete, and wipe clipboard history
- Backwards compatible with Cliphist TSV format
  - Import clipboard history from TSV (e.g., from `cliphist list`)
- Image preview (shows dimensions and format)
- Deduplication and entry limit control
- Text previews with customizable width
- Sensitive clipboard filtering via regex (see below)

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

## Usage

Command interface is only slightly different from Cliphist. In most cases, it
will be as simple as replacing `cliphist` with `stash` in your commands, aliases
or scripts.

> [!NOTE]
> It is not a priority to provide 1:1 backwards compatibility with Cliphist.
> While the interface is _almost_ identical, Stash chooses to build upon
> Cliphist's design and extend existing design choices. See
> [Migrating from Cliphist](#migrating-from-cliphist) for more details.

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

#### Sensitive Clipboard Filtering

Stash can be configured to avoid storing clipboard entries that match a
sensitive pattern, using a regular expression. This is useful for preventing
accidental storage of secrets, passwords, or other sensitive data. You don't
want sensitive data ending up in your persistent clipboard, right?

The filter can be configured in one of two ways:

- **Environment variable**: Set `STASH_SENSITIVE_REGEX` to a valid regex
  pattern. If clipboard text matches, it will not be stored.
- **Systemd LoadCredential**: If running as a service, you can provide a regex
  pattern via a credential file. For example, add to your `stash.service`:

  ```ini
  LoadCredential=clipboard_filter:/etc/stash/clipboard_filter
  ```

  The file `/etc/stash/clipboard_filter` should contain your regex pattern (no
  quotes). This is done automatically in the vendored Systemd service. Remember
  to set the appropriate file permissions if using this option.

The service will check the credential file first, then the environment variable.
If a clipboard entry matches the regex, it will be skipped and a warning will be
logged.

**Example regex to block common password patterns**:

- `(password|secret|api[_-]?key|token)[=: ]+[^\s]+`

## Tips & Tricks

### Migrating from Cliphist

Stash was designed to be a drop-in replacement for Cliphist, with only minor
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
stash import < cliphist.tsv
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
