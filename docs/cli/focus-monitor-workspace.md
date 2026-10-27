# focus-monitor-workspace

```
Focus the specified workspace on the target monitor

Usage: komorebic.exe focus-monitor-workspace <TARGET_MONITOR> <TARGET_WORKSPACE>

Arguments:
  <TARGET_MONITOR>
          Target monitor index (zero-indexed)

  <TARGET_WORKSPACE>
          Workspace index on the target monitor (zero-indexed)

Options:
  -h, --help
          Print help

```

Native mode (`"native_desktop_workspaces": true` in `komorebi.json`; requires restart): In native mode, the monitor selects the physical focus target and the workspace selects the global Windows desktop.
