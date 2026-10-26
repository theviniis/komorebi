---
hide:
  - toc
---

## Description

_komorebi_ only responds to [WinEvents](https://docs.microsoft.com/en-us/windows/win32/winauto/event-constants) and the
messages it receives on a dedicated socket.

_komorebic_ is a CLI that writes messages on _komorebi_'s socket.

_komorebi_ doesn't handle any keyboard or mouse inputs; a third party program (e.g.
[whkd](https://github.com/LGUG2Z/whkd)) is needed in order to translate keyboard and mouse events to _komorebic_ commands.

This architecture, popularised by [_bspwm_](https://github.com/baskerville/bspwm) on Linux and
[_yabai_](https://github.com/koekeishiya/yabai) on macOS, is outlined as follows:


```
          PROCESS                SOCKET
whkd/ahk  -------->  komorebic  <------>  komorebi
```

## Data Model

_komorebi_ holds a list of physical monitors.

A monitor is just a rectangle of the available work area which contains one or more virtual workspaces.

A workspace holds a list of containers.

A container is just a rectangle where one or more application windows can be displayed.

This means that:

- By default, each monitor has its own collection of workspaces. With `native_desktop_workspaces: true`, all monitors mirror the global Windows desktop list.
- Workspaces only know about containers and their dimensions, not about individual application windows
- Every application window must belong to a container, even if that container only contains one application window
- Many application windows can be stacked and cycled through in the same container within a workspace
- In native mode, new windows stay on the native desktop where they were created (no focus stealing or moving to current desktop)
- In native mode, workspace names are synchronized with native desktop names from Task View
- In native mode, `new-workspace` and `ensure-workspaces` commands are no-ops (workspaces are managed by Windows)
- In native mode, `swap-workspaces-with-monitor` returns an error because desktops are global
