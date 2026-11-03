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
tables, task lists, and emphasis are rendered, and ` ```mermaid ` blocks are drawn
as [diagrams](#mermaid-diagrams). The preview includes unsaved edits.

```toml
[keys.normal.space]
m = "markdown_reading_mode"
```

Automatic preview is off by default. Set `editor.markdown-reading-mode = true`
only if you want newly opened Markdown files to start in preview.

### Mermaid diagrams

The reading mode draws ` ```mermaid ` blocks with box drawing characters:

```
┌───────┐       ╭───────────╮
│ Start ├─ go ─►│ Next step │
└───────┘       ╰───────────╯
```

| Diagram | Notes |
| --- | --- |
| `flowchart` / `graph` | All directions, node shapes, labels, `&`, chained, dotted, thick and longer links. Subgraphs are not outlined; links to a subgraph point to a node named after it. Self loops are not drawn. |
| `sequenceDiagram` | Participants, actors, all message arrows, self messages, notes, `autonumber`, and `loop`/`alt`/`opt`/`par`/`critical`/`break` blocks. |
| `stateDiagram-v2` | States, start and end points, transitions with labels, descriptions, choice, fork and join. Composite states are flattened. |
| `classDiagram` | Classes with annotations, attributes and methods, relationships with cardinality and labels. |
| `erDiagram` | Entities with attributes, relationships with cardinality. |
| `pie` | Drawn as a bar chart with percentages. |
| `mindmap` | Drawn as a tree. |

Horizontal diagrams that do not fit are drawn top to bottom, and wider ones are cut off
at the edge of the view. Other diagram types, and diagrams that cannot be read, are shown
as source code.

Diagrams use `markup.diagram.border`, `.text`, `.title`, `.edge`, `.arrow`, `.label` and
`.muted`, falling back to `type`, `ui.text`, `markup.heading.1`, `punctuation`,
`operator`, `string` and `comment`.
