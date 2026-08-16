# burrow IPC

carrot listens on a unix socket at `$XDG_RUNTIME_DIR/carrot.$WAYLAND_DISPLAY.sock`.
One request per line in, one JSON reply per line out. `burrow` is the CLI in
front of it; anything below can be spoken directly by a shell.

All keys are kebab-case. All indices a client sees are 1-based; the wire
format for *dispatch* verbs is 0-based, and `burrow` converts.

## Requests

A request is one line of JSON, either a bare string (a query) or an object
(a dispatch action).

### Queries

| Command | Returns |
|---|---|
| `"outputs"` | array of [output records](#output-record) |
| `"monitors"` | pre-v2 output list, kept for compatibility |
| `"workspaces"` | array of [workspace records](#workspace-record) |
| `"windows"` | [window records](#window-record) on the active workspace |
| `"clients"` | window records across every workspace |
| `"binds"` | configured keybinds with their wire actions |
| `"errors"` | last config load's diagnostics |
| `"reload"` | reloads the config, returns `true` |
| `"dpms-on"` / `"dpms-off"` | returns `true` |
| `"dump-shadow"` | debug tree dump |

Replies are wrapped: `{"ok": <value>}` or `{"error": "<message>"}`.

### Dispatch

Any `Action` from the config's bind vocabulary, as a single-key object:

```json
{"focus-workspace": 2}
{"focus-workspace-rel": -1}
{"send-to-workspace": 4}
{"adjust-split-ratio": 0.05}
{"spawn": ["foot", "-e", "bash"]}
{"focus-dir": "left"}
"toggle-fullscreen"
```

`spawn` takes **argv**, not a shell string. Use `spawn-sh` for a shell line.

## Records

### Window record

```json
{
  "id": 42,
  "title": "~",
  "app-id": "foot",
  "pid": 1234,
  "workspace": 3,
  "output": "DP-3",
  "column": 7,
  "column-index": 1,
  "geometry": { "x": 0, "y": 0, "w": 1280, "h": 1376 },
  "state": {
    "focused": false,
    "fullscreen": false,
    "floating": false,
    "xwayland": false,
    "mapped": true
  }
}
```

- `id` is the surface uid: monotone, never reused within a session.
- `geometry` is the **painted** rect. A fullscreen window reports where it
  draws, not the tile it will return to.
- `column` is a stable id that survives a strip reorder. `column-index` is
  the column's current position in the strip and does not. Both are `null`
  on a dwindle workspace, which is a BSP tree with no column concept.

`clients` and `windows` additionally carry the pre-v2 flat keys `x`, `y`,
`w`, `h`, `floating`, `fullscreen`, `xwayland`, `mapped`, `focused`. They are
**deprecated**: read `geometry` and `state`. Events never carry them.

### Workspace record

```json
{
  "id": 3, "index": 3, "output": "DP-3",
  "active": true, "window-count": 4, "layout": "scrolling"
}
```

`layout` is `"scrolling"` or `"dwindle"`. `windows` is a deprecated alias for
`window-count`.

### Output record

```json
{
  "name": "DP-3",
  "x": 0, "y": 0, "width": 2560, "height": 1440,
  "scale": 1.0, "transform": 0, "refresh": 240,
  "workspace": 3, "focused": true,
  "reserved": { "top": 32, "bottom": 0, "left": 0, "right": 0 }
}
```

`reserved` is the layer-shell exclusive zone as edge insets from the output
rect, so the usable area is the rect minus these.

**`scale` is always `1.0` and `transform` is always `0`.** carrot implements
neither fractional scale nor output rotation yet. The fields exist so a shell
can read them once rather than being rewritten later; they are honest
constants, not placeholders to work around.

## Event stream

`"subscribe"` switches the connection to a line-delimited event stream.
Filter server-side with `{"subscribe": "windows,columns"}` (comma list). An
unknown category is an error, not a silent subscribe-to-nothing.

Categories: `windows`, `workspaces`, `outputs`, `columns`, `config`.
Omitted or empty means everything.

Every event object carries an `"event"` key naming it. The first line is
always one `state` snapshot; deltas follow.

```json
{"event":"state","workspaces":[...],"windows":[...],"outputs":[...]}
```

Only the categories subscribed to appear in the snapshot.

| Event | Category | Payload |
|---|---|---|
| `state` | any | `workspaces`, `windows`, `outputs` arrays |
| `window-opened` | windows | `window`: full record |
| `window-focused` | windows | `window`: full record |
| `window-fullscreen` | windows | `window`: full record (read `state.fullscreen`) |
| `window-closed` | windows | `window`: `id`, `title`, `app-id`, `workspace`, `column`, `column-index` |
| `workspace-activated` | workspaces | `workspace`: full record |
| `workspace-moved` | workspaces | `workspace`: full record (its `output` changed) |
| `column-layout-changed` | columns | `workspace` index, `columns`: `[{column, column-index, windows:[id]}]` |
| `outputs-changed` | outputs | `outputs`: full array |
| `config` | config | config load status; replayed to late subscribers |

`window-closed` carries a reduced record because the window is gone from the
tree by the time it fires. The column it *was* in is captured before removal,
so a shell can retire it from the right place.

Events carry whole records so a consumer never re-queries. A workspace switch
costs one event, not one event plus N queries.

### Performance

Events are built once per category and fanned to every subscriber that asked
for it. Nothing is serialized when no subscriber wants that category, so an
unsubscribed event costs a borrow and a bitmask test on the compositor's hot
path.

## Compatibility

The event stream changed shape in v0.1.4. Before it, events were dispatched
by *which key was present* (`{"window-opened": {...}}`) and carried title and
app-id only. There is no compatibility mode: every event now has an `"event"`
key and a complete record. See the changelog.

Queries kept their top-level shapes; every v2 field on them is additive.
