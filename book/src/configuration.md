# Configuration

To override global configuration parameters, create a `config.toml` file located in your config directory:

- Linux and Mac: `~/.config/helix/config.toml`
- Windows: `%AppData%\helix\config.toml`

> 💡 You can easily open the config file by typing `:config-open` within Helix normal mode.

Example config:

```toml
theme = "onedark"

[editor]
line-number = "relative"
mouse = false

[editor.cursor-shape]
insert = "bar"
normal = "block"
select = "underline"

[editor.file-picker]
hidden = false
```

You can use a custom configuration file by specifying it with the `-c` or
`--config` command line argument, for example `hx -c path/to/custom-config.toml`.
You can reload the config file by issuing the `:config-reload` command. Alternatively, on Unix operating systems, you can reload it by sending the USR1
signal to the Helix process, such as by using the command `pkill -USR1 hx`.

Finally, you can have a `config.toml` and a `languages.toml` local to a project by putting it under a `.helix` directory in your repository.
Its settings will be merged with the configuration directory and the built-in configuration.

## Markdown reading mode (local build)

Bind `Space m` to open a rendered, read-only preview of the current `.md` file.
Editing keys and paste are ignored while reading. Use `j`/`k`
or arrows to scroll, `Ctrl-d`/`Ctrl-u` for half pages, Page Down/Page Up for
full pages, and `gg`/`G` for the beginning/end.

Press `g` then `w` to reveal the source and start the usual word-jump command.
Press `q`, Escape, or `Space m` to reveal the source without jumping. After
scrolling or navigating, the source opens at the section being read. Use `[`/`]`
to move between headings, `/` to search, `n`/`N` for next/previous matches, and
`?` to show the controls. Comments are hidden, prose reflows, and fenced code,
tables, task lists, and emphasis are rendered. The preview includes unsaved edits.

```toml
[keys.normal.space]
m = "markdown_reading_mode"
```

Automatic preview is off by default. Set `editor.markdown-reading-mode = true`
only if you want newly opened Markdown files to start in preview.
