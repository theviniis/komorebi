# move-workspace-to-monitor

```
Move the focused workspace to the specified monitor

Usage: komorebic.exe move-workspace-to-monitor <TARGET>

Arguments:
  <TARGET>
          Target index (zero-indexed)

Options:
  -h, --help
          Print help

```

Native mode (`"native_desktop_workspaces": true` in `komorebi.json`; requires restart): This command returns an explanatory error in native mode because Windows desktops are global.
