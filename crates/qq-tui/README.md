# qq-tui

Terminal user interface and client-side state for QQ.

## Throwaway chat prototypes

The `tui_prototype` example compares three interaction models against the same
synthetic streaming conversation. It deliberately lives outside the library so
the selected ideas can be reimplemented against the real client state rather
than turning prototype structure into a compatibility constraint.

```sh
nix develop -c cargo run -p qq-tui --example tui_prototype
```

Use `F1`-`F3` to switch concepts and `Tab` to change reasoning visibility.
`Ctrl-T` opens the session tree; move with `Up`/`Down`, focus with `Enter`, and
close it with `Esc`. Outside the tree, `Esc` focuses the parent session. Type a
prompt and press `Enter` to start another synthetic stream. `Ctrl-C` exits.
