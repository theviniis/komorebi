# Native Windows desktops

This branch requires Windows 11 24H2 build **26100.2605** or newer. Startup checks the detected Windows build before COM initialization and reports both detected and minimum builds when unsupported. The native backend uses the pinned winvd 0.0.49 dependency and internal Windows desktop interfaces.

## Desktop ownership

Create, remove, rename and reorder desktops in **Task View**. Komorebi mirrors the ordered native list; it never creates or removes desktops. All monitors have the same desktop count and focused desktop. Layouts, padding, stacks, floating windows and monitor-specific configuration remain separate for each monitor.

Indices are zero-based positions in the current native list. Internally, stable IDs preserve surviving workspace configuration when desktops move. Switching desktops uses Windows, without hiding/restoring an entire workspace. Visibility operations remain necessary for stacks and explicit monocle/maximize actions.

Ctrl+Win+Left/Right are native Windows shortcuts. Leave these combinations unbound in whkd so they work even while komorebi is stopped. Existing bindings that invoke `cycle-workspace` intercept the Windows shortcut and depend on the daemon; remove/comment those bindings and restart whkd. Other shortcuts may continue invoking `cycle-workspace`.

## Command compatibility

| Command | Behavior |
|---|---|
| `focus-workspace N` / `focus-workspaces N` | Switch native desktop N globally; preserve the selected physical monitor. |
| `focus-monitor-workspace M N` | Validate both destinations, select native desktop N and monitor M. |
| `cycle-workspace previous/next` | Focus the adjacent native desktop; no wrap at either end, matching Windows. |
| `focus-last-workspace` | Return to the previous native desktop, globally. |
| `focus-named-workspace NAME` | Resolve the first native desktop with this name and switch globally. |
| `move-to-workspace N` / `send-to-workspace N` | Move all stack members or the active floating window through the native backend; move follows, send does not. |
| Container movement to monitor/workspace | Preserve one global desktop selection; transferring between monitors does not give each monitor its own workspace focus. |
| `workspace-name M N NAME` | Rename native desktop N globally. M remains a compatibility argument. |
| `new-workspace`, `close-workspace`, `ensure-workspaces`, `ensure-named-workspaces` | Compatibility no-ops with a warning. Use Task View to manage the list. |
| `swap-workspaces-with-monitor`, `move-workspace-to-monitor`, cycle workspace movement between monitors | Unsupported; return an explanatory error. |

Named layout/padding/rule commands apply to the currently selected monitor on the named native desktop. Names are global; they no longer identify a separate monitor-owned workspace. If native names are duplicated, the first index wins.

The SocketMessage JSON tags and argument shapes remain compatible. Current socket and notification schemas are shipped as [schema.socket.json](../schema.socket.json) and [schema.notification.json](../schema.notification.json). Regenerate all schemas with `just jsonschema`; generation only prints schemas and does not send commands to a running daemon.

## Configuration

A monitor's `workspaces` entries configure existing native desktop positions. Extra entries are ignored with a warning rather than creating desktops. New desktops use defaults until their configuration is loaded/reloaded. Monitor cache templates are also limited to the native count.

Windows desktop names are authoritative and are reflected in the state and bar. Legacy workspace `name` fields remain accepted for configuration compatibility, but loading/reloading configuration and reconnecting monitors never rename native desktops. Rename desktops in Task View or deliberately invoke `workspace-name`.

Workspace rules still redirect applications explicitly, using native desktop movement. Invalid desktop or monitor destinations fail before membership changes. Initial-only rules are recorded after success and do not undo later user moves. Permanent rules continue enforcing their explicit target.

The bar reads the global native desktop focus and count. It safely handles temporarily incomplete monitor data instead of indexing an absent workspace.

## Startup, restoration and hotplug

Initial enumeration adopts eligible windows on their actual monitor and native desktop, including native-cloaked windows on another desktop. Enumeration does not maximize/unmaximize or focus applications merely to discover them.

Restoration queries native membership for every live saved window before committing the restored tree. It preserves windows discovered since the snapshot, keeps valid stacks/floating/monocle/maximize membership, and does not select a saved desktop or silently move windows to the last desktop. Saved workspaces beyond the current native count do not create desktops: their windows are adopted on their actual destinations. Closed HWNDs are omitted. A live window with an unavailable native destination causes an explicit error before replacement.

Current configured padding/layout options/default caches remain authoritative; matching container resize data is restored. Ephemeral preselection is cleared so its stale index cannot remove a live restored container. Legacy state without the global native fields can still be read. A different saved physical monitor count is rejected explicitly; it does not replace the current managed state.

Monitor hotplug preserves layout caches by physical identity and native workspace IDs. Windows already managed on an attached monitor take precedence over stale cache membership. Reconciliation queries current physical/native destinations, preserves background-desktop windows, and never minimizes applications or switches desktop merely to rebuild monitor state. Failure preserves the previous association/cache for retry. Display-index and size preferences are reapplied.

## Validation still required before completion

Automated fake-backend tests do not replace interactive validation on supported Windows. Record the OS build, monitor count and result for each scenario in the [execution plan](plans/2026-10-25-native-desktop-migration-completion.md):

- Switch through commands and Win+Ctrl+arrows; check global focus, bar and borders.
- Create, rename, reorder and remove desktops in Task View, including an intermediate and the current desktop.
- Move stacks and floating windows with and without following; also move individual windows in Task View.
- Open/manage an application on a background desktop without bringing it to the current desktop.
- Disconnect/reconnect a second monitor, including a desktop-count change while it is disconnected.
- Restart/restore with a different native desktop count; keep newly opened windows and saved windows on their real desktops.
- Perform ten consecutive switches without duplicate, lost or incorrectly hidden windows.

The migration is complete only after every acceptance item in that plan has recorded evidence, including manual scenarios and release builds.
