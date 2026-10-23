# Native Desktop Workspaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** komorebi espelha os desktops virtuais do Windows numa lista global compartilhada por todos os monitores, trocando workspaces com `switch_desktop` em vez de esconder janelas.

**Architecture:** Cada monitor mantém `Vec<Workspace>` com um item por desktop; o índice focado é único em `WindowManager`. `winvd` fica encapsulado em `virtual_desktop.rs` atrás de uma trait `DesktopBackend` injetável.

**Tech Stack:** Rust 2024, crate `windows` 0.62, `winvd` 0.0.49, testes `#[cfg(test)]` por módulo (`cargo test -p komorebi`).

**Spec:** `docs/superpowers/specs/2026-10-22-native-desktop-workspaces-design.md`

## Global Constraints

- Build mínimo **26100.2605** (Windows 11 24H2). Abaixo disso: erro explícito e saída antes de inicializar COM.
- Nunca criar nem remover desktops. `new-workspace`, `ensure-workspaces` e `ensure-named-workspaces` são no-op com `tracing::warn!`.
- Janela nova permanece no desktop onde nasceu; é tileada lá, sem receber foco e sem mudar `focused_desktop_idx`.
- `workspace_idx` significa sempre índice de desktop nativo.
- Nenhuma chamada `hide`/`restore` de workspace no caminho de troca de desktop. `HidingBehaviour` sobrevive apenas para stacks.
- Nenhum módulo além de `virtual_desktop.rs` importa `winvd`.

## Review Focus

1. Trocar de desktop gera cloak/uncloak automático; o komorebi não pode interpretar isso como ação do usuário nem remover a janela do gerenciamento.
2. Janela criada num desktop não visível não pode receber foco, não pode mover `focused_desktop_idx`, e não pode ser tratada como órfã pelo reaper.
3. Bordas overlay pertencem ao desktop onde nasceram; após a troca precisam ser redesenhadas no desktop novo.
4. `komorebic state` com monitores de comprimentos diferentes depois de criar um desktop no Task View.
5. Cliente antigo enviando `new-workspace` esperando workspaces novos — deve receber no-op e continuar funcionando.

---

### Task 1: Piso de versão

**Files:**
- Create: `komorebi/src/version_gate.rs`
- Modify: `komorebi/src/lib.rs` (adicionar `pub mod version_gate;`)
- Modify: `komorebi/src/main.rs` (primeira linha de `main()`)
- Test: testes inline em `komorebi/src/version_gate.rs`

**Interfaces:**
- Produces: `pub const MIN_BUILD: u32 = 26100;`, `pub const MIN_UBR: u32 = 2605;`, `pub fn is_supported_build(build: u32, ubr: u32) -> bool`, `pub fn require_supported_windows() -> eyre::Result<()>`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn build_floor_is_26100_2605() {
    assert!(is_supported_build(26100, 2605));
    assert!(!is_supported_build(26100, 2604));
    assert!(!is_supported_build(26099, 9999));
    assert!(is_supported_build(26200, 0));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi version_gate`
Expected: FAIL — `cannot find function is_supported_build`.

- [ ] **Step 3: Implement `is_supported_build` and `require_supported_windows`**

`is_supported_build` = `build > MIN_BUILD || (build == MIN_BUILD && ubr >= MIN_UBR)`. `require_supported_windows` lê `CurrentBuildNumber` (string) e `UBR` (u32) de `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` via `winreg`, e devolve `Err` com build detectado e mínimo exigido no texto.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komorebi version_gate`
Expected: PASS.

- [ ] **Step 5: Wire into `main()`**

`require_supported_windows()?` como primeira instrução de `main()`, antes de `WindowsApi::allow_set_foreground_window`.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/version_gate.rs komorebi/src/lib.rs komorebi/src/main.rs
git commit -m "feat: require windows 11 24h2 for native desktop support"
```

---

### Task 2: Camada `virtual_desktop`

**Files:**
- Create: `komorebi/src/virtual_desktop.rs`
- Modify: `komorebi/Cargo.toml` (adicionar `winvd = "0.0.49"`)
- Modify: `komorebi/src/lib.rs`
- Test: testes inline em `komorebi/src/virtual_desktop.rs`

**Interfaces:**
- Consumes: nada de tarefas anteriores.
- Produces:

```rust
pub trait DesktopBackend: Send + Sync {
    fn desktop_count(&self) -> eyre::Result<usize>;
    fn current_desktop_idx(&self) -> eyre::Result<usize>;
    fn switch_desktop(&self, idx: usize) -> eyre::Result<()>;
    fn desktop_idx_for_window(&self, hwnd: isize) -> eyre::Result<usize>;
    fn move_window_to_desktop(&self, hwnd: isize, idx: usize) -> eyre::Result<()>;
    fn desktop_name(&self, idx: usize) -> eyre::Result<Option<String>>;
    fn set_desktop_name(&self, idx: usize, name: &str) -> eyre::Result<()>;
}
pub struct WinvdBackend;
pub fn backend() -> Arc<dyn DesktopBackend>;
pub fn set_backend(b: Option<Arc<dyn DesktopBackend>>);
pub fn desktop_idx_from_winvd(v: u32) -> usize;
pub fn desktop_idx_to_winvd(idx: usize) -> eyre::Result<i32>;
```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn desktop_index_conversions_roundtrip() {
    assert_eq!(desktop_idx_from_winvd(0), 0);
    assert_eq!(desktop_idx_to_winvd(2).unwrap(), 2);
    assert!(desktop_idx_to_winvd(usize::MAX).is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi virtual_desktop`
Expected: FAIL — módulo não existe.

- [ ] **Step 3: Implement the module**

`WinvdBackend` delega a `winvd::{get_desktop_count, get_current_desktop, switch_desktop, get_desktop_by_window, move_window_to_desktop, get_desktop}`. Toda função converte `winvd::Error` em `eyre` com contexto e loga `tracing::error!` antes de propagar. `desktop_name` retorna `Ok(None)` para nome vazio.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komorebi virtual_desktop && cargo build -p komorebi`
Expected: PASS e build limpo.

- [ ] **Step 5: Commit**

```bash
git add komorebi/src/virtual_desktop.rs komorebi/src/lib.rs komorebi/Cargo.toml
git commit -m "feat: add virtual_desktop wrapper around winvd"
```

---

### Task 3: Índice de desktop global no modelo

**Files:**
- Modify: `komorebi/src/window_manager.rs` (struct `WindowManager`, `focused_workspace_idx`, `focused_workspace`, `focused_workspace_mut`)
- Modify: `komorebi/src/monitor.rs` (novo `sync_workspace_count`)
- Modify: `komorebi/src/state.rs` (refletir índice global)
- Test: `komorebi/src/monitor.rs`, `komorebi/src/window_manager.rs`

**Interfaces:**
- Consumes: `virtual_desktop::backend()`, `DesktopBackend::desktop_count`.
- Produces: `WindowManager.focused_desktop_idx: usize`, `WindowManager.desktop_count: usize`, `WindowManager::sync_desktop_count(&mut self) -> eyre::Result<()>`, `Monitor::sync_workspace_count(&mut self, count: usize)`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn sync_workspace_count_grows_and_shrinks_preserving_first() {
    let mut m = monitor::new(0, Rect::default(), Rect::default(),
        "M".into(), "D".into(), "DID".into(), Some("SN".into()));
    m.sync_workspace_count(3);
    assert_eq!(m.workspaces().len(), 3);
    m.sync_workspace_count(1);
    assert_eq!(m.workspaces().len(), 1);
    m.sync_workspace_count(1);
    assert_eq!(m.workspaces().len(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi sync_workspace_count`
Expected: FAIL — método inexistente.

- [ ] **Step 3: Implement `sync_workspace_count` and `sync_desktop_count`**

`sync_workspace_count` usa `Ring::resize`. `sync_desktop_count` lê `desktop_count` do backend, aplica `sync_workspace_count` em todos os monitores, ajusta `focused_desktop_idx = min(focused_desktop_idx, count-1)`, e grava `self.desktop_count`. Documentar a invariante em comentário: todo monitor tem `len() == desktop_count` e `focused_idx == focused_desktop_idx`.

- [ ] **Step 4: Route focused-workspace accessors through the global index**

`WindowManager::focused_workspace_idx()` retorna `Ok(self.focused_desktop_idx)`; `focused_workspace()`/`focused_workspace_mut()` indexam esse valor no monitor focado.

- [ ] **Step 5: Run tests**

Run: `cargo test -p komorebi`
Expected: PASS, ajustando testes existentes que assumiam índice por monitor.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/monitor.rs komorebi/src/state.rs
git commit -m "refactor(wm): single global focused workspace index"
```

---

### Task 4: Troca de workspace via desktop nativo

**Files:**
- Modify: `komorebi/src/window_manager.rs` (`focus_workspace` → `focus_desktop`)
- Modify: `komorebi/src/process_command.rs` (braços `FocusWorkspaceNumber`, `FocusWorkspaceNumbers`, `FocusMonitorWorkspaceNumber`)
- Test: `komorebi/src/window_manager.rs`

**Interfaces:**
- Consumes: `DesktopBackend::switch_desktop`, `WindowManager.focused_desktop_idx`.
- Produces: `WindowManager::focus_desktop(&mut self, idx: usize) -> eyre::Result<()>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn focus_desktop_moves_every_monitor_to_the_same_index() {
    // backend falso com desktop_count() == 3
    let (mut wm, _ctx) = setup_window_manager_with_fake_desktops(3);
    wm.focus_desktop(2).unwrap();
    assert_eq!(wm.focused_desktop_idx, 2);
    for m in wm.monitors() {
        assert_eq!(m.focused_workspace_idx(), 2);
        assert_eq!(m.workspaces().len(), 3);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi focus_desktop`
Expected: FAIL — método inexistente.

- [ ] **Step 3: Implement `focus_desktop`**

Valida `idx < self.desktop_count`, chama `backend().switch_desktop(idx)`, grava `focused_desktop_idx`, chama `self.update_focused_workspace(false, true)`. `focus_workspace` passa a delegar para `focus_desktop`.

- [ ] **Step 4: Rewrite the socket handlers**

`FocusWorkspaceNumber(n)` → `focus_desktop(n)`. `FocusWorkspaceNumbers(n)` → idem (sem loop por monitor). `FocusMonitorWorkspaceNumber(m, n)` → `focus_monitor(m)` + `focus_desktop(n)`.

- [ ] **Step 5: Run tests and manual check**

Run: `cargo test -p komorebi`, depois `komorebic focus-workspace 2` numa máquina com 3 desktops.
Expected: testes PASS; o Windows troca para o desktop 2 e o layout é aplicado.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/process_command.rs
git commit -m "feat(wm): switch workspaces by switching native desktops"
```

---

### Task 5: Eventos de desktop

**Files:**
- Create: `komorebi/src/desktop_listener.rs`
- Modify: `komorebi/src/window_manager_event.rs` (novas variantes)
- Modify: `komorebi/src/process_event.rs` (braços novos no `match`)
- Modify: `komorebi/src/main.rs` (iniciar listener)
- Test: `komorebi/src/process_event.rs`

**Interfaces:**
- Consumes: `WindowManager::sync_desktop_count`, `WindowManager.focused_desktop_idx`.
- Produces: variantes `WindowManagerEvent::DesktopChanged { new: usize, old: usize }`, `DesktopCountChanged`, `DesktopMoved { desktop: usize, old: usize, new: usize }`; `pub fn listen_for_desktop_events(tx: Sender<WindowManagerEvent>)`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn desktop_changed_updates_index_without_switching_back() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    wm.process_event(WindowManagerEvent::DesktopChanged { new: 1, old: 0 }).unwrap();
    assert_eq!(wm.focused_desktop_idx, 1);
    assert_eq!(fake.switch_calls(), 0); // não reenvia switch_desktop
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi desktop_changed`
Expected: FAIL — variante inexistente.

- [ ] **Step 3: Implement the listener**

Thread dedicada; guarda o `winvd::DesktopEventThread` num `OnceLock` estático para não ser dropado. Mapeia `DesktopChanged{new,old}`, `DesktopCreated`/`DesktopDestroyed`/`DesktopMoved`, `DesktopNameChanged`. `WindowChanged` é ignorado (já coberto pelos WinEvents).

- [ ] **Step 4: Implement the `process_event` arms**

`DesktopChanged` → atualiza índice + `update_focused_workspace`, sem chamar `switch_desktop`. `DesktopCountChanged` → `sync_desktop_count()` + `update_focused_workspace`. `DesktopMoved` → permuta os workspaces `old`/`new` em todos os monitores.

- [ ] **Step 5: Start the listener from `main()`**

Antes de `listen_for_events(wm.clone())`.

- [ ] **Step 6: Run tests and manual check**

Run: `cargo test -p komorebi desktop`, depois Win+Ctrl+→ e conferir `komorebic state`.
Expected: testes PASS; o índice focado acompanha o desktop nativo.

- [ ] **Step 7: Commit**

```bash
git add komorebi/src/desktop_listener.rs komorebi/src/window_manager_event.rs komorebi/src/process_event.rs komorebi/src/main.rs
git commit -m "feat: mirror native desktop events into window manager"
```

---

### Task 6: Janela nova fica no desktop de origem

**Files:**
- Modify: `komorebi/src/window_manager.rs` (novo `desktop_idx_for_new_window`)
- Modify: `komorebi/src/process_event.rs` (braço `Manage`)
- Test: `komorebi/src/process_event.rs`

**Interfaces:**
- Consumes: `DesktopBackend::desktop_idx_for_window`, `focused_desktop_idx`.
- Produces: `WindowManager::desktop_idx_for_new_window(&mut self, window: Window) -> eyre::Result<usize>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn window_born_on_other_desktop_is_tiled_without_stealing_focus() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    fake.set_window_desktop(42, 2);
    wm.focus_desktop(0).unwrap();
    wm.desktop_idx_for_new_window(Window::from(42)).unwrap();
    assert_eq!(wm.focused_desktop_idx, 0);
    assert_eq!(fake.focus_calls_for(42), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi new_window_desktop`
Expected: FAIL — método inexistente.

- [ ] **Step 3: Implement `desktop_idx_for_new_window`**

Consulta o backend. Se o índice for igual a `focused_desktop_idx`, retorna-o (fluxo normal). Senão, ainda retorna o índice de origem; quem chamou decide inserir sem foco.

- [ ] **Step 4: Wire into the `Manage` arm**

Quando o índice retornado difere do focado, inserir no workspace daquele índice sem `Window::focus`. Regras de `workspace_matching_rules` continuam aplicando depois, via `move_window_to_desktop`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p komorebi new_window_desktop`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/process_event.rs
git commit -m "feat(wm): keep new windows on their native desktop"
```

---

### Task 7: Mover janela entre desktops

**Files:**
- Modify: `komorebi/src/window_manager.rs` (`move_container_to_workspace`, `move_workspace_to_monitor`)
- Modify: `komorebi/src/process_command.rs` (`SwapWorkspacesToMonitorNumber`, `CycleMoveContainerToWorkspace`)
- Test: `komorebi/src/window_manager.rs`

**Interfaces:**
- Consumes: `DesktopBackend::move_window_to_desktop`, `WindowManager.focus_desktop`.
- Produces: comportamento novo em `move_container_to_workspace`; erro explícito em `move_workspace_to_monitor`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn move_container_moves_every_window_to_target_desktop() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    // container com hwnds 10 e 11 no desktop 0
    wm.move_container_to_workspace(2, false, None).unwrap();
    assert_eq!(fake.moved(), vec![(10, 2), (11, 2)]);
    assert_eq!(wm.focused_desktop_idx, 0); // follow = false
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi move_container_moves_every`
Expected: FAIL.

- [ ] **Step 3: Implement the move**

Coletar todos os hwnds do container (tiling, stack, flutuantes, maximizado, monocle) antes de remover, chamar `backend().move_window_to_desktop(hwnd, idx)` para cada um, inserir no workspace de destino, e só focar o desktop destino se `follow == true`.

- [ ] **Step 4: Rewrite the impossible commands**

`move_workspace_to_monitor` e `SwapWorkspacesToMonitorNumber` retornam `eyre::bail!("swapping workspaces between monitors is not supported with native desktops")`. `CycleMoveContainerToWorkspace` cicla sobre `desktop_count` em vez de `monitor.workspaces().len()`.

- [ ] **Step 5: Run tests**

Run: `cargo test -p komorebi`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/process_command.rs
git commit -m "feat(wm): move containers between native desktops"
```

---

### Task 8: CLI, nomes e no-ops

**Files:**
- Modify: `komorebi/src/window_manager.rs` (`ensure_workspaces_for_monitor`, `ensure_named_workspaces_for_monitor`, `set_workspace_name`, `new_workspace`)
- Modify: `komorebi/src/process_command.rs`
- Modify: `komorebic/src/main.rs`
- Test: `komorebi/src/window_manager.rs`

**Interfaces:**
- Consumes: `DesktopBackend::set_desktop_name`, `DesktopBackend::desktop_name`.
- Produces: `WindowManager.workspace_names: HashMap<usize, String>` global (sai de `Monitor`).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn workspace_name_is_written_to_the_native_desktop() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(2);
    wm.set_workspace_name(0, 1, "web".to_string()).unwrap();
    assert_eq!(fake.desktop_name(1), Some("web".to_string()));
    for m in wm.monitors() {
        assert_eq!(m.workspaces()[1].name.as_deref(), Some("web"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi workspace_name_is_written`
Expected: FAIL.

- [ ] **Step 3: Implement name propagation**

`set_workspace_name` chama o backend e grava o nome em todos os monitores e em `workspace_names`. Na inicialização e em `DesktopNameChanged`, os nomes vêm do backend.

- [ ] **Step 4: Turn workspace creation into no-ops**

`new_workspace`, `ensure_workspaces_for_monitor` e `ensure_named_workspaces_for_monitor` passam a apenas `tracing::warn!("desktops are managed by the task view")` e retornam `Ok(())`.

- [ ] **Step 5: Hide the unusable CLI command**

`swap-workspaces-with-monitor` recebe `#[clap(hide = true)]` e retorna erro explicativo.

- [ ] **Step 6: Run tests**

Run: `cargo test -p komorebi && cargo build --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/process_command.rs komorebic/src/main.rs
git commit -m "feat(cli): reflect native desktops in workspace commands"
```

---

### Task 9: Remover a máquina de virtual desktop antiga

**Files:**
- Modify: `komorebi/src/lib.rs` (remover `current_virtual_desktop`, `CURRENT_VIRTUAL_DESKTOP`, `VirtualDesktopNotification`)
- Modify: `komorebi/src/process_event.rs` (remover o bloco de "ignorar eventos fora do desktop")
- Modify: `komorebi/src/window_manager.rs` (remover `virtual_desktop_id`)
- Modify: `komorebi/src/state.rs` (remover `current_virtual_desktop_id`)
- Modify: `komorebi/src/static_config.rs`
- Modify: `komorebi-bar/src/widgets/komorebi.rs`
- Test: `komorebi/src/state.rs`

**Interfaces:**
- Consumes: `WindowManagerEvent::DesktopChanged`, `DesktopCountChanged` (Task 5).
- Produces: `NotificationEvent::Desktop(DesktopNotification)` substituindo `VirtualDesktop`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn serialized_state_has_no_legacy_virtual_desktop_fields() {
    let json = serde_json::to_string(&GlobalState::default()).unwrap();
    assert!(!json.contains("current_virtual_desktop_id"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi serialized_state_has_no_legacy`
Expected: FAIL.

- [ ] **Step 3: Delete the legacy machinery**

Remover `current_virtual_desktop()`, `CURRENT_VIRTUAL_DESKTOP`, `virtual_desktop_id`, `current_virtual_desktop_id`, `VirtualDesktopNotification`, e o bloco de `process_event` que retornava cedo fora do desktop associado. No lugar, `sync_desktop_count()` no início de `process_event`.

- [ ] **Step 4: Update the bar**

`komorebi-bar` troca a assinatura de `VirtualDesktop` por `Desktop` e re-renderiza em `DesktopChanged`.

- [ ] **Step 5: Run tests**

Run: `cargo test -p komorebi && cargo build --workspace`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: remove registry-based virtual desktop tracking"
```

---

### Task 10: Enxugar hide/restore

**Files:**
- Modify: `komorebi/src/monitor.rs` (`load_focused_workspace`)
- Modify: `komorebi/src/workspace.rs` (`hide`)
- Modify: `komorebi/src/window_manager.rs` (`restore_all_windows`)
- Test: `komorebi/src/monitor.rs`

**Interfaces:**
- Consumes: nada novo.
- Produces: `Monitor::load_focused_workspace` sem esconder workspaces não focados.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn load_focused_workspace_does_not_hide_other_workspaces() {
    let mut m = Monitor::new(0, Rect::default(), Rect::default(),
        "M".into(), "D".into(), "DID".into(), Some("SN".into()));
    m.sync_workspace_count(2);
    assert_eq!(m.workspaces()[1].focused_container().map(|_| ()), None);
    m.load_focused_workspace(false).unwrap();
    // nenhum hwnd entra em HIDDEN_HWNDS por causa de troca de workspace
    assert!(HIDDEN_HWNDS.lock().is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi does_not_hide_other_workspaces`
Expected: FAIL.

- [ ] **Step 3: Stop hiding on workspace switch**

`load_focused_workspace` só restaura e posiciona o workspace focado. `Workspace::hide` permanece apenas para uso em stacks via `Container::load_focused_window`.

- [ ] **Step 4: Stop touching other desktops on shutdown**

`restore_all_windows` limita-se ao desktop atual; janelas em outros desktops ficam como estão.

- [ ] **Step 5: Run tests and manual check**

Run: `cargo test -p komorebi`. Manual: 10 trocas de desktop seguidas; nenhuma janela some, nenhuma borda fica órfã.
Expected: PASS nos testes; comportamento limpo no manual.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/monitor.rs komorebi/src/workspace.rs komorebi/src/window_manager.rs
git commit -m "refactor: stop hiding windows when switching workspaces"
```

---

### Task 11: Restauração de estado

**Files:**
- Modify: `komorebi/src/window_manager.rs` (`apply_state`)
- Test: `komorebi/src/window_manager.rs`

**Interfaces:**
- Consumes: `WindowManager::sync_desktop_count`.
- Produces: `apply_state` reconciliado com `desktop_count`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn applying_state_with_fewer_workspaces_than_desktops_keeps_all_desktops() {
    let (mut wm, _fake) = setup_window_manager_with_fake_desktops(3);
    let mut state = State::from(&wm);
    state.monitors.elements_mut()[0].workspaces.elements_mut().truncate(2);
    wm.apply_state(state);
    assert_eq!(wm.monitors()[0].workspaces().len(), 3);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p komorebi applying_state_with_fewer`
Expected: FAIL.

- [ ] **Step 3: Reconcile before applying**

`apply_state` chama `sync_desktop_count` primeiro; aplica `min(state_len, desktop_count)` workspaces; o excedente do estado é descartado e as janelas dele voltam como flutuantes no último desktop válido.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p komorebi applying_state`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add komorebi/src/window_manager.rs
git commit -m "fix(wm): reconcile restored state with native desktops"
```

---

### Task 12: Documentação e schema

**Files:**
- Modify: `README.md` (requisito de versão)
- Modify: `docs/installation.md`, `docs/design.md`, `docs/usage/*.md`
- Regenerate: `schema.json`, `schema.bar.json`

**Interfaces:**
- Consumes: comportamento implementado nas tarefas 1–11.
- Produces: documentação e schema coerentes.

- [ ] **Step 1: Update the version floor**

`README.md`: "Windows 11 24H2 (build 26100.2605) and above". Nota explícita de que Windows 10 não é mais suportado.

- [ ] **Step 2: Document the model**

Em `docs/design.md` e nos docs de uso: lista global de desktops, sem workspaces por monitor, `new-workspace`/`ensure-workspaces` como no-op, janela nova fica no desktop de origem, `swap-workspaces-with-monitor` removido.

- [ ] **Step 3: Regenerate and commit schemas**

Regenerar `schema.json` e `schema.bar.json` com o alvo do repo (o mesmo usado no release), sem mudanças manuais.

- [ ] **Step 4: Commit**

```bash
git add README.md docs schema.json schema.bar.json
git commit -m "docs: document native desktop workspaces"
```

---

## Dependências

- 1 → 2 → 3 bloqueiam todas as outras.
- 4–7 dependem de 3.
- 8–9 dependem de 4.
- 10–11 dependem de 6.
- 12 é a última.
