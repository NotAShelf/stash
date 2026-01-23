<!-- markdownlint-disable MD033 -->

<h1 id="header" align="center">
    <pre>Stash</pre>
</h1>

<div align="center">
    <a alt="CI Status" href="https://github.com/NotAShelf/stash/actions">
        <img
          src="https://github.com/NotAShelf/stash/actions/workflows/rust.yml/badge.svg"
          alt="Build Status"
        />
    </a>
    <a alt="Dependencies" href="https://deps.rs/repo/github/notashelf/stash">
        <img
          src="https://deps.rs/repo/github/notashelf/stash/status.svg"
          alt="Dependency Status"
        />
    </a>
</div>

<div align="center">
  Lightweight Wayland clipboard "manager" with fast persistent history and
  robust multi-media support. Stores and previews clipboard entries (text, images)
  on the clipboard with a neat TUI and advanced scripting capabilities.
</div>

<div align="center">
  <br/>
  <a href="#features">Features</a><br/>
  <a href="#installation">Installation</a> | <a href="#usage">Usage</a><br/>
  <a href="#tips--tricks">Tips and Tricks</a>
  <br/>
</div>

## Features

Stash is a feature-rich, yet simple and lightweight clipboard management utility
with many features such as but not necessarily limited to:

- Automatic MIME detection for stored entries
- Fast persistent storage using SQLite
- List, search, decode, delete, and wipe clipboard history with ease
- Backwards compatible with Cliphist TSV format
  - Import clipboard history from TSV (e.g., from `cliphist list`)
- Image preview (shows dimensions and format)
- Text previews with customizable width
- De-duplication, whitespace prevention and entry limit control
- Automatic clipboard monitoring with `stash watch`
  - Configurable auto-expiry of old entries in watch mode as a safety buffer
- Drop-in replacement for `wl-clipboard` tools (`wl-copy` and `wl-paste`)
- Sensitive clipboard filtering via regex (see below)
- Sensitive clipboard filtering by application (see below)

See [usage section](#usage) for more details.

## Installation

### With Nix

Nix is the recommended way of downloading Stash. You can install it using Nix
flakes using `nix profile add` if on non-nixos or add Stash as a flake input if
you are on NixOS.

```nix
{
  # Add Stash to your inputs like so
  inputs.stash.url = "github:NotAShelf/stash";

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

If you want to give Stash a try before you switch to it, you may also run it one
time with `nix run`.

```sh
nix run github:NotAShelf/stash -- watch # start the watch daemon
```

### Without Nix

[GitHub Releases]: https://github.com/notashelf/stash/releases

You can also install Stash on any of your systems _without_ using Nix. New
releases are made when a version gets tagged, and are available under
[GitHub Releases]. To install Stash on your system without Nix, either:

- Download a tagged release from [GitHub Releases] for your platform and place
  the binary in your `$PATH`. Instructions may differ based on your
  distribution, but generally you want to download the built binary from
  releases and put it somewhere like `/usr/bin` or `~/.local/bin` depending on
  your distribution.
- Build and install from source with Cargo:

  ```bash
  cargo install --git https://github.com/notashelf/stash
  ```

## Usage

> [!NOTE]
> It is not a priority to provide 1:1 backwards compatibility with Cliphist.
> While the interface is _almost_ identical, Stash chooses to build upon
> Cliphist's design and extend existing design choices. See
> [Migrating from Cliphist](#migrating-from-cliphist) for more details.

The command interface of Stash is _only slightly_ different from Cliphist. In
most cases, you may simply replace `cliphist` with `stash` and your commands,
aliases or scripts will continue to work as intended.

Some of the commands allow further fine-graining with flags such as `--type` or
`--format` to allow specific input and output specifiers. See `--help` for
individual subcommands if in doubt.

<!-- markdownlint-disable MD013 -->

```console
$ stash help
Wayland clipboard manager

Usage: stash [OPTIONS] [COMMAND]

Commands:
  store   Store clipboard contents
  list    List clipboard history
  decode  Decode and output clipboard entry by id
  delete  Delete clipboard entry by id (if numeric), or entries matching a query (if not). Numeric arguments are treated as ids. Use --type to specify explicitly
  db      Database management operations
  import  Import clipboard data from stdin (default: TSV format)
  watch   Start a process to watch clipboard for changes and store automatically
  help    Print this message or the help of the given subcommand(s)

Options:
      --max-items <MAX_ITEMS>
          Maximum number of clipboard entries to keep [default: 18446744073709551615]
      --max-dedupe-search <MAX_DEDUPE_SEARCH>
          Number of recent entries to check for duplicates when storing new clipboard data [default: 20]
      --preview-width <PREVIEW_WIDTH>
          Maximum width (in characters) for clipboard entry previews in list output [default: 100]
      --db-path <DB_PATH>
          Path to the `SQLite` clipboard database file [env: STASH_DB_PATH=]
      --excluded-apps <EXCLUDED_APPS>
          Application names to exclude from clipboard history [env: STASH_EXCLUDED_APPS=]
      --ask
          Ask for confirmation before destructive operations
  -v, --verbose...
          Increase logging verbosity
  -q, --quiet...
          Decrease logging verbosity
  -h, --help
          Print help
  -V, --version
          Print version
```

<!-- markdownlint-enable MD013 -->

### Store an entry

```bash
echo "some clipboard text" | stash store
```

### List entries

```bash
stash list
```

Stash list will list all entries in an interactive TUI that allows navigation
and copying/deleting entries. This behaviour is EXCLUSIVE TO TTYs and Stash will
display entries in Cliphist-compatible TSV format in Bash scripts. You may also
enforce the output format with `stash list --format <tsv | json>`.

You may also view your clipboard _with the addition of expired entries_, i.e.,
entries that have reached their TTL and are marked as expired, using the
`--expired` flag as `stash list --expired`. Expired entries are not cleaned up
when using this flag, allowing you to inspect them before running cleanup.

### Decode an entry by ID

```bash
stash decode <input ID>
```

> [!TIP]
> Decoding from dmenu-compatible tools:
>
> ```bash
> stash list | tofi | stash decode
> ```

### Delete entries matching a query

```bash
stash delete --type [id | query] <text or ID>
```

By default stash will try to guess the type of an entry, but this may not be
desirable for all users. If you wish to be explicit, pass `--type` to
`stash delete`.

### Delete multiple entries by ID (from a file or stdin)

```bash
stash delete --type id < ids.txt
```

### Wipe all entries

> [!WARNING]
> This command is deprecated, and will be removed in v0.4.0. Use `stash db wipe`
> instead.

```bash
stash wipe
```

### Database management

Stash provides a `db` subcommand for database maintenance operations:

```bash
stash db wipe [--expired] [--ask]
stash db vacuum
stash db stats
```

- `stash db wipe`: Remove all entries from the database. Use `--expired` to only
  wipe expired entries instead of all entries. Requires `--ask` confirmation by
  default.
- `stash db vacuum`: Optimize the database using SQLite's VACUUM command,
  reclaiming space and improving performance.
- `stash db stats`: Display database statistics including total/active/expired
  entry counts, storage size, and page information. This is provided purely for
  convenience and the rule of the cool.

### Watch clipboard for changes and store automatically

```bash
stash watch
```

This runs a daemon that monitors the clipboard and stores new entries
automatically. This is designed as an alternative to shelling out to
`wl-paste --watch` inside a Systemd service or XDG autostart. You may find a
premade Systemd service in `contrib/`. Packagers are encouraged to vendor the
service unless adding their own.

#### Automatic Clipboard Clearing on Expiration

When `stash watch` is running and a clipboard entry expires, Stash will detect
if the current clipboard still contains that expired content and automatically
clear it. This prevents stale data from remaining in your clipboard after an
entry has expired from history.

> [!NOTE]
> This behavior only applies when the watch daemon is actively running. Manual
> expiration or deletion of entries will not clear the clipboard.

### MIME Type Preference for Watch

`stash watch` supports a `--mime-type` (short `-t`) option that lets you
prioritise which MIME type the daemon should request from the clipboard when
multiple representations are available.

- `any` (default): Request any available representation (current behaviour).
- `text`: Prefer text representations (e.g. `text/plain`, `text/html`).
- `image`: Prefer image representations (e.g. `image/png`, `image/jpeg`) so that
  image copies from browsers or file managers are stored as images rather than
  HTML fragments.

Example: prefer images when running the watch daemon

```bash
stash watch --mime-type image
```

This is useful when copying images from browsers or file managers where the
clipboard may offer both HTML and image representations; selecting `image` will
ask the compositor for image data first. Most users will be fine using the
default value (`any`) but in the case your browser (or other applications!)
regularly misrepresent data, you might wish to prioritize a different type.

### Options

Some commands take additional flags to modify Stash's behavior. See each
commands `--help` text for more details. The following are generally standard:

- `--db-path <path>`: Custom database path
- `--max-items <N>`: Maximum number of entries to keep (oldest trimmed)
- `--max-dedupe-search <N>`: Deduplication window size
- `--preview-width <N>`: Text preview max width for `list`
- `--version`: Print the current version and exit

### Sensitive Clipboard Filtering

Stash can be configured to avoid storing clipboard entries that match a
sensitive pattern, using a regular expression. This is useful for preventing
accidental storage of secrets, passwords, or other sensitive data. You don't
want sensitive data ending up in your persistent clipboard, right?

The filter can be configured in one of three ways, as part of two separate
features.

#### Clipboard Filtering by Entry Regex

This can be configured in one of two ways. You can use the **environment
variable** `STASTH_SENSITIVE_REGEX` to a valid regex pattern, and if the
clipboard text matches the regex it will not be stored. This can be used for
trivial secrets such as but not limited to GitHub tokens or secrets that follow
a rule, e.g. a prefix. You would typically set this in your `~/.bashrc` or
similar but in some cases this might be a security flaw.

The safer alternative to this is using **Systemd LoadCrediental**. If Stash is
running as a Systemd service, you can provide a regex pattern using a crediental
file. For example, add to your `stash.service`:

```dosini
LoadCredential=clipboard_filter:/etc/stash/clipboard_filter
```

The file `/etc/stash/clipboard_filter` should contain your regex pattern (no
quotes). This is done automatically in the
[vendored Systemd service](./contrib/stash.service). Remember to set the
appropriate file permissions if using this option.

The service will check the credential file first, then the environment variable.
If a clipboard entry matches the regex, it will be skipped and a warning will be
logged.

> [!TIP]
> **Example regex to block common password patterns**:
>
> `(password|secret|api[_-]?key|token)[=: ]+[^\s]+`
>
> For security reasons, you are recommended to use the regex only for generic
> tokens that follow a specific rule, for example a generic prefix or suffix.

#### Clipboard Filtering by Application Class

Stash allows blocking an entry from the persistent history if it has been copied
from certain applications. This depends on the `use-toplevel` feature flag and
uses the the `wlr-foreign-toplevel-management-v1` protocol for precise focus
detection. While this feature flag is enabled (the default) you may use
`--excluded-apps` in, e.g., `stash watch` or set the `STASH_EXCLUDED_APPS`
environment variable to block entries from persisting in the database if they
are coming from your password manager for example. The entry is still copied to
the clipboard, but it will never be put inside the database.

This is a more robust alternative to using the regex method above, since you
likely do not want to catch your passwords with a regex. Simply pass your
password manager's **window class** to `--excluded-apps` and your passwords will
be only copied to the clipboard.

> [!TIP]
> **Example startup command for Stash daemon**:
>
> `stash --excluded-apps Bitwarden watch`

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
  crate and provides its own `wl-copy` and `wl-paste` binaries.

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

Here are some other tips for Stash that are worth documenting. If you have
figured out something new, e.g. a neat shell trick, feel free to add it here!

1. You may use `stash list` to view your clipboard history in an interactive
   TUI. This is obvious if you have ever ran the command, but here are some
   things that you might not have known.
   - `stash list` displays the TUI _only_ if the user is in an interactive TTY.
     E.g. if it's a Bash script, `stash list` **will output TSV**.
   - You can change the format with `--format` to e.g. JSON but you can also
     force a TSV format inside an interactive session with `--format tsv`.
   - `stash list` displays the mime type for newly recorded entries, but it will
     not be able to display them for entries imported by Cliphist since Cliphist
     never made a record of this data.
2. You can pipe `cliphist list --db ~/.cache/cliphist/db` to
   `stash import --type tsv` to mimic importing from STDIN.

   ```bash
   cliphist list --db ~/.cache/cliphist/db | stash import
   ```

3. Stash provides its own implementation of `wl-copy` and `wl-paste` commands
   backed by `wl-clipboard-rs`. Those implementations are backwards compatible
   with `wl-clipboard`, and may be used as **drop-in** replacements. The default
   build wrapper in `build.rs` links `stash` to `stash-copy` and `stash-paste`,
   which are also available as `wl-copy` and `wl-paste` respectively. The Nix
   package automatically links those to `$out/bin` for you, which means they are
   installed by default but other package managers may need additional steps by
   the packagers. While building from source, you may link
   `target/release/stash` manually.

### Entry Expiration

Stash supports time-to-live (TTL) for clipboard entries. When an entry's
expiration time is reached, it is marked as expired rather than immediately
deleted. This allows for inspection of expired entries and automatic clipboard
cleanup.

#### How Expiration Works

When `stash watch` is running with `--expire-after`, it monitors the clipboard
and processes expired entries periodically. Upon expiration:

1. The entry's `is_expired` flag is set to `1` in the database
2. If the current clipboard content matches the expired entry, Stash clears the
   clipboard to prevent pasting stale data
3. Expired entries are excluded from normal list operations unless `--expired`
   is specified

> [!NOTE]
> By default, entries do not expire. Use `stash watch --expire-after DURATION`
> to enable expiration (e.g., `--expire-after 24h` for 24-hour TTL).

#### Viewing Expired Entries

Use `stash list --expired` to include expired entries in the output. This is
useful for:

- Inspecting what has expired from your clipboard history
- Verifying that sensitive data has been properly expired
- Debugging expiration behavior

```bash
# View all entries including expired ones
stash list --expired

# View expired entries in JSON format
stash list --expired --format json
```

#### Cleaning Up Expired Entries

The watch daemon automatically cleans up expired entries when it processes them.
For manual cleanup, use:

```bash
# Remove all expired entries from the database
stash db wipe --expired
```

> [!NOTE]
> If you have a large number of expired entries, consider running
> `stash db vacuum` afterward to reclaim disk space.

#### Automatic Clipboard Clearing

When `stash watch` is running and an entry expires, Stash checks if the current
clipboard still contains that expired content. If it matches, Stash clears the
clipboard automatically. This prevents accidentally pasting outdated content.

> [!TIP]
> This behavior only applies when the watch daemon is actively running. Manual
> expiration or deletion of entries will not clear the clipboard.

#### Database Maintenance

Stash uses SQLite for persistent storage. Over time, deleted entries and
fragmentation can affect performance. Use the `stash db` command to maintain
your database:

- **Check statistics**: `stash db stats` shows entry counts and storage usage.
  Use this to monitor growth and decide when to clean up.
- **Remove expired entries**: `stash db wipe --expired` removes entries that
  have reached their TTL. The daemon normally handles this, but this is useful
  for manual cleanup.
- **Optimize storage**: `stash db vacuum` runs SQLite's VACUUM command to
  reclaim space and defragment the database. This is safe to run periodically.

It is recommended to run `stash db vacuum` occasionally (e.g., monthly) to keep
the database compact, especially after deleting many entries.

## Attributions

My thanks go first to [@YaLTeR](https://github.com/YaLTeR/) for the
[wl-clipboard-rs](https://github.com/YaLTeR/wl-clipboard-rs) crate. Stash is
powered by [several crates](./Cargo.toml), but none of them were as detrimental
in Stash's design process.

Additional thanks to my testers, who have tested earlier versions of Stash and
provided feedback. Thank you :)

## License

This project is made available under Mozilla Public License (MPL) version 2.0.
See [LICENSE](LICENSE) for more details on the exact conditions. An online copy
is provided [here](https://www.mozilla.org/en-US/MPL/2.0/).
