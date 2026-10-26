# cycle-workspace

```
Focus the workspace in the given cycle direction

Usage: komorebic.exe cycle-workspace <CYCLE_DIRECTION>

Arguments:
  <CYCLE_DIRECTION>
          [possible values: previous, next]

Options:
  -h, --help
          Print help

```

Native mode (`"native_desktop_workspaces": true` in `komorebi.json`; requires restart): In native mode, this switches the global Windows desktop and stops at either end of the list. Traditional mode cycles the focused monitor’s workspaces.
