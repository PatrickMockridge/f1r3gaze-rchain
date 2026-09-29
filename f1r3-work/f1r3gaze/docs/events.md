# Events

A listener installed with `el!("listen", type, *ch, options, *sub)` receives,
for each event, the pair

```
(type, fields)
```

where `type` is the event type as a string and `fields` is a map. The fields
below are **guaranteed**: every host (the native browser, the reach tier)
delivers at least these, with these meanings. A host may deliver more, and
later versions may add fields, so a page should name the fields it uses and
end its pattern with a remainder:

```
for (@(_, {"target": *t, "x": x, "y": y ..._}) <= clicks) { ... }
for (@("keydown", {"key": k, "mods": mods ..._}) <= keys) { ... }
for (@(_, {"value": v ..._}) <= edits) { ... }
```

A pattern without `...` matches only a map with exactly its keys; it stops
matching the moment a host adds a field, and an unmatched event is not an
error: it simply waits. Use exact patterns only for data whose shape you
control.

## Guaranteed fields

| events | fields |
| --- | --- |
| every event | `target`: the element the event is aimed at, as a name the page can send on; `type`: the event type |
| `click`, `dblclick`, `contextmenu`, `pointerdown`, `pointerup`, `pointermove`, `pointercancel`, `pointerenter`, `pointerleave`, `pointerover`, `pointerout`, `mousedown`, `mouseup`, `mousemove`, `mouseenter`, `mouseleave`, `mouseover`, `mouseout`, `touchstart`, `touchmove`, `touchend`, `touchcancel` | `x`, `y`: page coordinates, integers; `button`: integer; `mods`: list of `"shift"`, `"ctrl"`, `"alt"`, `"meta"` |
| `keydown`, `keyup`, `keypress` | `key`, `code`: strings as in UI Events; `mods`: as above; `repeat`: boolean |
| `input` | `value`: the control's current value, a string |

All numbers are integers: no field carries a float, so every host delivers
bit-identical values and replay stays exact.

Known additions a host may make today: the reach tier also sets `value` on
`change`. Nothing may rely on an unguaranteed field.
