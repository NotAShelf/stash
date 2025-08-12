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
