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

## Tips & Tricks

### Migrating from Cliphist

[Cliphist]: https://github.com/sentriz/cliphist

Stash is designed to be backwards compatible with [Cliphist]. Though for
brevity, I have elected to skip automatic database migration. Which means you
must handle the migration yourself, with one simple command.

```bash
$ cliphist list --db ~/.cache/cliphist/db | stash --import-tsv
# > Imported 750 records from TSV into SQLite database.
```

Alternatively, you may first export from Cliphist and _then_ import the
database.

```bash
$ cliphist list --db ~/.cache/cliphist/db > cliphist.tsv
$ stash --import-tsv < cliphist.tsv
# > Imported 750 records from TSV into SQLite database.
```
