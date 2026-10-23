# Native Desktop Workspaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** komorebi espelha os desktops virtuais do Windows numa lista global compartilhada por todos os monitores, trocando workspaces com `switch_desktop` em vez de esconder janelas.

**Architecture:** Cada monitor mantém `Vec<Workspace>` com um item por desktop; o índice focado é único em `WindowManager`. `winvd` fica encapsulado em `virtual_desktop.rs` atrás de uma trait `DesktopBackend` injetável, com backend falso para os testes.

**Tech Stack:** Rust 2024, crate `windows` 0.62, `winvd` 0.0.49, testes `#[cfg(test)]` por módulo (`cargo test -p komorebi`).

**Spec:** `docs/superpowers/specs/2026-10-22-native-desktop-workspaces-design.md`

## Global Constraints

- Build mínimo **26100.2605** (Windows 11 24H2). Abaixo disso: erro explícito e saída antes de inicializar COM.
- Nunca criar nem remover desktops. `new-workspace`, `ensure-workspaces` e `ensure-named-workspaces` são no-op com `tracing::warn!`.
- Janela nova permanece no desktop onde nasceu; é tileada lá, sem receber foco e sem mudar `focused_desktop_idx`.
- `workspace_idx` significa sempre índice de desktop nativo.
- Nenhuma chamada `hide`/`restore` de workspace no caminho de troca de desktop. `HidingBehaviour` sobrevive apenas para stacks.
- Nenhum módulo além de `virtual_desktop.rs` importa `winvd`.
- `Ring` não tem `resize`; resize é sempre `monitor.workspaces_mut().resize(n, Workspace::default())` sobre o `VecDeque`.

## Review Focus

1. Trocar de desktop gera cloak/uncloak automático em massa; o komorebi não pode tratar isso como ação do usuário nem desgerenciar a janela. (teste na Task 10)
2. Janelas em desktops não visíveis não podem ser reportadas como órfãs pelo reaper. (teste na Task 12)
3. Bordas overlay pertencem ao desktop onde nasceram; após a troca precisam ser recriadas no desktop novo. (teste na Task 11)
4. `apply_state` com quantidade de workspaces diferente da de desktops não pode perder janelas nem deixar monitores com comprimentos divergentes. (teste na Task 14)
5. Cliente antigo enviando `new-workspace` esperando workspaces novos deve receber no-op e o processo continuar íntegro. (teste na Task 8)

---

### Task 1: Piso de versão

**Files:**
- Create: `komorebi/src/version_gate.rs`
- Modify: `komorebi/src/lib.rs` (`pub mod version_gate;`), `komorebi/src/main.rs` (primeira instrução de `main()`)

**Interfaces:**
- Produces: `pub const MIN_BUILD: u32 = 26100`, `pub const MIN_UBR: u32 = 2605`, `pub fn is_supported_build(build: u32, ubr: u32) -> bool`, `pub fn require_supported_windows() -> eyre::Result<()>`

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

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p komorebi version_gate`
Expected: FAIL — `cannot find function is_supported_build`.

- [ ] **Step 3: Implement `is_supported_build` and `require_supported_windows`**

`is_supported_build` = `build > MIN_BUILD || (build == MIN_BUILD && ubr >= MIN_UBR)`. `require_supported_windows` lê `CurrentBuildNumber` (string) e `UBR` (u32) de `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` via `winreg`, e devolve `Err` com build detectado e mínimo exigido no texto. Valor ilegível conta como não suportado.

- [ ] **Step 4: Run it and watch it pass**

Run: `cargo test -p komorebi version_gate`
Expected: PASS.

- [ ] **Step 5: Call it from `main()`**

`require_supported_windows()?` antes de `WindowsApi::allow_set_foreground_window`.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/version_gate.rs komorebi/src/lib.rs komorebi/src/main.rs
git commit -m "feat: require windows 11 24h2 for native desktop support"
```

---

### Task 2: Camada `virtual_desktop` e backend falso

**Files:**
- Create: `komorebi/src/virtual_desktop.rs`
- Modify: `komorebi/Cargo.toml` (`winvd = "0.0.49"`), `komorebi/src/lib.rs`

**Interfaces:**
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

pub enum DesktopEventKind {
    Changed { new: usize, old: usize },
    CountChanged,
    Moved { desktop: usize, old: usize, new: usize },
    Renamed { desktop: usize },
}
pub fn spawn_event_thread(cb: impl Fn(DesktopEventKind) + Send + 'static) -> eyre::Result<()>;
```

`spawn_event_thread` é a única função que toca `winvd::listen_desktop_events`; mantém o `DesktopEventThread` num `OnceLock` estático e traduz para `DesktopEventKind` antes de chamar `cb`.

- Produces também, em `#[cfg(test)] pub mod testing`: `FakeDesktopBackend` com `new(count: usize)`, `set_window_desktop(hwnd, idx)`, `switch_calls() -> usize`, `moved() -> Vec<(isize, usize)>`, `names() -> HashMap<usize, String>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn desktop_index_conversions_roundtrip() {
    assert_eq!(desktop_idx_from_winvd(0), 0);
    assert_eq!(desktop_idx_to_winvd(2).unwrap(), 2);
    assert!(desktop_idx_to_winvd(usize::MAX).is_err());
}

#[test]
fn fake_backend_records_operations() {
    let fake = FakeDesktopBackend::new(3);
    fake.set_window_desktop(7, 2);
    assert_eq!(fake.desktop_count().unwrap(), 3);
    assert_eq!(fake.desktop_idx_for_window(7).unwrap(), 2);
    fake.switch_desktop(1).unwrap();
    assert_eq!(fake.switch_calls(), 1);
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p komorebi virtual_desktop`
Expected: FAIL — módulo inexistente.

- [ ] **Step 3: Implement the module**

`WinvdBackend` delega a `winvd::{get_desktop_count, get_current_desktop, switch_desktop, get_desktop_by_window, move_window_to_desktop, get_desktop}`; `desktop_name` usa `get_desktop(idx).get_name()` e devolve `Ok(None)` para string vazia. Todo `winvd::Error` vira `eyre` com contexto. `backend()` devolve o backend instalado por `set_backend` ou, na ausência, um `OnceLock` com `WinvdBackend`. `FakeDesktopBackend` guarda estado em `Mutex`/atômicos.

- [ ] **Step 4: Run and watch them pass**

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
- Modify: `komorebi/src/window_manager.rs` (struct `WindowManager`; `focused_workspace_idx`, `focused_workspace`, `focused_workspace_mut`, `focused_workspace_idx_for_monitor_idx`)
- Modify: `komorebi/src/monitor.rs` (`sync_workspace_count`)
- Modify: `komorebi/src/state.rs`

**Interfaces:**
- Consumes: `virtual_desktop::backend()`, `DesktopBackend::desktop_count`.
- Produces: `WindowManager.focused_desktop_idx: usize`, `WindowManager.desktop_count: usize`, `WindowManager::sync_desktop_count(&mut self) -> eyre::Result<()>`, `Monitor::sync_workspace_count(&mut self, count: usize)`, `Monitor::set_focused_workspace_idx(&mut self, idx: usize)`.

- [ ] **Step 1: Write the failing tests**

```rust
// monitor.rs
#[test]
fn sync_workspace_count_grows_and_shrinks_keeping_index_zero() {
    let mut m = Monitor::new(0, Rect::default(), Rect::default(),
        "M".into(), "D".into(), "DID".into(), Some("SN".into()));
    m.sync_workspace_count(3);
    assert_eq!(m.workspaces().len(), 3);
    m.sync_workspace_count(1);
    assert_eq!(m.workspaces().len(), 1);
    m.sync_workspace_count(1);
    assert_eq!(m.workspaces().len(), 1);
}

#[test]
fn set_focused_workspace_idx_updates_the_monitor_ring() {
    let mut m = Monitor::new(0, Rect::default(), Rect::default(),
        "M".into(), "D".into(), "DID".into(), Some("SN".into()));
    m.sync_workspace_count(3);
    m.set_focused_workspace_idx(2);
    assert_eq!(m.focused_workspace_idx(), 2);
}

// window_manager.rs
#[test]
fn sync_desktop_count_keeps_monitors_uniform() {
    let (mut wm, _fake) = setup_window_manager_with_fake_desktops(3);
    wm.sync_desktop_count().unwrap();
    assert_eq!(wm.desktop_count, 3);
    for m in wm.monitors() {
        assert_eq!(m.workspaces().len(), 3);
    }
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p komorebi sync_workspace_count sync_desktop_count`
Expected: FAIL — métodos inexistentes.

- [ ] **Step 3: Implement `sync_workspace_count` and `sync_desktop_count`**

`sync_workspace_count` = `self.workspaces_mut().resize(count.max(1), Workspace::default())`. `sync_desktop_count` lê `backend().desktop_count()?`, aplica em todos os monitores, grava `self.desktop_count`, e ajusta `self.focused_desktop_idx = self.focused_desktop_idx.min(count.saturating_sub(1))`, chamando `Monitor::set_focused_workspace_idx` em todos os monitores. Comentar a invariante: todo monitor tem `workspaces().len() == desktop_count` e `focused_workspace_idx()` igual ao índice global.

`Monitor::set_focused_workspace_idx` apenas chama `self.workspaces.focus(idx)` (o `Ring` continua sendo o armazenamento do índice dentro do monitor); a fonte da verdade é o campo do `WindowManager`.

- [ ] **Step 4: Route the accessors through the global index**

`focused_workspace_idx()` devolve `Ok(self.focused_desktop_idx)`. `focused_workspace()`/`focused_workspace_mut()` indexam esse valor em `monitor.workspaces().get(self.focused_desktop_idx)` (sem consultar `Ring::focused`). `focused_workspace_idx_for_monitor_idx(_)` devolve o índice global, ignorando o `Ring` daquele monitor.

- [ ] **Step 5: Keep the per-monitor ring index in sync**

`sync_desktop_count` e `focus_desktop` chamam `Monitor::set_focused_workspace_idx(self.focused_desktop_idx)` em todos os monitores, para que `monitor.focused_workspace()` (usado por código que não passa pelo `WindowManager`) continue coerente.

- [ ] **Step 6: Add the test helper**

`setup_window_manager_with_fake_desktops(count: usize) -> (WindowManager, Arc<FakeDesktopBackend>)` em `#[cfg(test)] mod tests` de `window_manager.rs`: instala o fake via `set_backend`, cria o `WindowManager`, insere um monitor de teste e chama `sync_desktop_count`.

- [ ] **Step 7: Run the whole suite**

Run: `cargo test -p komorebi`
Expected: PASS, corrigindo os testes que dependiam de `Ring<Workspace>` focado por monitor.

- [ ] **Step 8: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/monitor.rs komorebi/src/state.rs
git commit -m "refactor(wm): single global focused workspace index"
```

---

### Task 4: Troca de workspace via desktop nativo

**Files:**
- Modify: `komorebi/src/window_manager.rs:3803` (`focus_workspace` → `focus_desktop`)
- Modify: `komorebi/src/process_command.rs:1217-1268`

**Interfaces:**
- Consumes: `DesktopBackend::switch_desktop`, `WindowManager.focused_desktop_idx`, `desktop_count`.
- Produces: `WindowManager::focus_desktop(&mut self, idx: usize) -> eyre::Result<()>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn focus_desktop_moves_every_monitor_to_the_same_index() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    wm.focus_desktop(2).unwrap();
    assert_eq!(wm.focused_desktop_idx, 2);
    assert_eq!(fake.switch_calls(), 1);
    for m in wm.monitors() {
        assert_eq!(m.focused_workspace_idx(), 2);
        assert_eq!(m.workspaces().len(), 3);
    }
}

#[test]
fn focus_desktop_rejects_out_of_range_index() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    assert!(wm.focus_desktop(3).is_err());
    assert_eq!(fake.switch_calls(), 0);
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p komorebi focus_desktop`
Expected: FAIL — método inexistente.

- [ ] **Step 3: Implement `focus_desktop`**

Valida `idx < self.desktop_count` (senão `bail!`), chama `backend().switch_desktop(idx)?`, grava `focused_desktop_idx = idx`, e chama `self.update_focused_workspace(false, true)`. `WindowManager::focus_workspace` passa a ser um alias que delega para `focus_desktop`.

- [ ] **Step 4: Rewrite the socket handlers**

`FocusWorkspaceNumber(n)` → `focus_desktop(n)`; `FocusWorkspaceNumbers(n)` → idem (sem loop por monitor); `FocusMonitorWorkspaceNumber(m, n)` → `focus_monitor(m)` e depois `focus_desktop(n)`.

- [ ] **Step 5: Run tests, then check by hand**

Run: `cargo test -p komorebi`, depois `komorebic focus-workspace 2` com 3 desktops abertos.
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
- Modify: `komorebi/src/window_manager_event.rs` (novas variantes + `Display`), `komorebi/src/process_event.rs`, `komorebi/src/main.rs`

**Interfaces:**
- Consumes: `virtual_desktop::spawn_event_thread`, `WindowManager::sync_desktop_count`, `focused_desktop_idx`.
- Produces: variantes `WindowManagerEvent::DesktopChanged { new: usize, old: usize }`, `WindowManagerEvent::DesktopCountChanged`, `WindowManagerEvent::DesktopMoved { desktop: usize, old: usize, new: usize }`, `WindowManagerEvent::DesktopRenamed { desktop: usize }`; `pub fn listen_for_desktop_events(tx: Sender<WindowManagerEvent>) -> eyre::Result<()>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn desktop_changed_updates_index_without_switching_back() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    wm.process_event(WindowManagerEvent::DesktopChanged { new: 1, old: 0 }).unwrap();
    assert_eq!(wm.focused_desktop_idx, 1);
    assert_eq!(fake.switch_calls(), 0);
}

#[test]
fn desktop_moved_permutes_workspaces_on_every_monitor() {
    let (mut wm, _fake) = setup_window_manager_with_fake_desktops(3);
    wm.process_event(WindowManagerEvent::DesktopMoved { desktop: 0, old: 0, new: 2 }).unwrap();
    for m in wm.monitors() {
        assert_eq!(m.workspaces().len(), 3);
    }
    assert_eq!(wm.focused_desktop_idx, 2);
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p komorebi desktop_changed desktop_moved`
Expected: FAIL — variantes inexistentes.

- [ ] **Step 3: Implement the listener**

`desktop_listener.rs` chama `virtual_desktop::spawn_event_thread` com um closure que traduz `DesktopEventKind` em `WindowManagerEvent` e envia pelo canal `tx`. Nenhum `winvd::` aparece neste arquivo — o mapeamento já vem pronto do módulo `virtual_desktop` (Task 2).

- [ ] **Step 4: Implement the `process_event` arms**

`DesktopChanged` → grava `focused_desktop_idx` e `update_focused_workspace(false, true)`, sem chamar `switch_desktop`. `DesktopCountChanged` → `sync_desktop_count()` e `update_focused_workspace(false, true)`. `DesktopMoved` → move o `Workspace` da posição `old` para `new` em todos os monitores, atualiza `focused_desktop_idx` para o novo índice do desktop que estava focado. `DesktopRenamed` → relê `backend().desktop_name(idx)?` e grava em `workspace.name` de todos os monitores no índice correspondente.

- [ ] **Step 5: Start the listener from `main()`**

`listen_for_desktop_events(...)` antes de `listen_for_events(wm.clone())`, no mesmo canal de eventos.

- [ ] **Step 6: Run tests, then check by hand**

Run: `cargo test -p komorebi desktop`, depois Win+Ctrl+→ e `komorebic state`.
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
    let idx = wm.desktop_idx_for_new_window(Window::from(42)).unwrap();
    assert_eq!(idx, 2);
    assert_eq!(wm.focused_desktop_idx, 0);
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p komorebi window_born_on_other_desktop`
Expected: FAIL — método inexistente.

- [ ] **Step 3: Implement `desktop_idx_for_new_window`**

Devolve `backend().desktop_idx_for_window(window.hwnd)?` limitado a `desktop_count.saturating_sub(1)`. Não altera foco nem estado.

- [ ] **Step 4: Wire it into the `Manage` arm**

Se o índice devolvido for diferente de `focused_desktop_idx`, inserir a janela no workspace daquele índice do monitor correspondente **sem** chamar `Window::focus`. Se for igual, o fluxo atual continua intacto. Regras de `workspace_matching_rules` aplicam depois, via `move_window_to_desktop`.

- [ ] **Step 5: Run tests, then check by hand**

Run: `cargo test -p komorebi window_born`, depois abrir um app com foco num desktop diferente do dele.
Expected: PASS; a janela aparece tileada no desktop de origem quando você troca para lá, e o desktop atual não muda.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/process_event.rs
git commit -m "feat(wm): keep new windows on their native desktop"
```

---

### Task 7: Mover container entre desktops

**Files:**
- Modify: `komorebi/src/window_manager.rs:1737` (`move_container_to_workspace`), `move_workspace_to_monitor`
- Modify: `komorebi/src/process_command.rs` (`SwapWorkspacesToMonitorNumber`, `CycleMoveContainerToWorkspace`)

**Interfaces:**
- Consumes: `DesktopBackend::move_window_to_desktop`, `WindowManager::focus_desktop`.
- Produces: `move_container_to_workspace` movendo janelas via backend; `move_workspace_to_monitor` retornando erro.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn move_container_moves_every_window_to_target_desktop() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    // container com hwnds 10 e 11 no desktop 0
    wm.move_container_to_workspace(2, false, None).unwrap();
    assert_eq!(fake.moved(), vec![(10, 2), (11, 2)]);
    assert_eq!(wm.focused_desktop_idx, 0);
}

#[test]
fn moving_a_workspace_to_another_monitor_is_rejected() {
    let (mut wm, _fake) = setup_window_manager_with_fake_desktops(3);
    assert!(wm.move_workspace_to_monitor(0).is_err());
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p komorebi move_container_moves_every moving_a_workspace`
Expected: FAIL.

- [ ] **Step 3: Implement the move**

Antes de remover, coletar todos os hwnds do container (tiling, stack, flutuantes, maximizado, monocle). Chamar `backend().move_window_to_desktop(hwnd, idx)?` para cada um, inserir o container no workspace de destino, e chamar `focus_desktop(idx)` apenas se `follow == true`.

- [ ] **Step 4: Reject the commands with no equivalent**

`move_workspace_to_monitor` e o braço `SwapWorkspacesToMonitorNumber` passam a `eyre::bail!("swapping workspaces between monitors is not supported with native desktops")`. `CycleMoveContainerToWorkspace` cicla sobre `self.desktop_count` em vez de `monitor.workspaces().len()`.

- [ ] **Step 5: Run tests**

Run: `cargo test -p komorebi`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/process_command.rs
git commit -m "feat(wm): move containers between native desktops"
```

---

### Task 8: Nomes de desktop e no-ops de CLI

**Files:**
- Modify: `komorebi/src/window_manager.rs:3567-3655`, `komorebi/src/process_command.rs:1396-1407`, `komorebic/src/main.rs`
- Modify: `komorebi/src/monitor.rs` (`workspace_names` sai de `Monitor`, mas `Monitor::focus_workspace` perde o uso dele), `komorebi/src/state.rs:295`, `komorebi/src/window_manager.rs:3818` (`monitor_workspace_index_by_name`)

**Interfaces:**
- Consumes: `DesktopBackend::{set_desktop_name, desktop_name}`.
- Produces: `WindowManager.workspace_names: HashMap<usize, String>` (global) e `WindowManager::monitor_workspace_index_by_name(&self, name: &str) -> Option<(usize, usize)>` devolvendo `(focused_monitor_idx, workspace_idx)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn workspace_name_is_written_to_the_native_desktop() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(2);
    wm.set_workspace_name(0, 1, "web".to_string()).unwrap();
    assert_eq!(fake.names().get(&1).map(String::as_str), Some("web"));
    for m in wm.monitors() {
        assert_eq!(m.workspaces()[1].name.as_deref(), Some("web"));
    }
}

#[test]
fn new_workspace_is_a_noop() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(2);
    wm.new_workspace().unwrap();
    assert_eq!(wm.desktop_count, 2);
    assert_eq!(fake.desktop_count().unwrap(), 2);
    assert_eq!(wm.monitors()[0].workspaces().len(), 2);
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p komorebi workspace_name_is_written new_workspace_is_a_noop`
Expected: FAIL.

- [ ] **Step 3: Implement name propagation**

`set_workspace_name(monitor_idx, workspace_idx, name)` chama `backend().set_desktop_name(workspace_idx, &name)?`, grava em `self.workspace_names` e em `workspace.name` de **todos** os monitores. Na inicialização (`init`/`sync_desktop_count`), os nomes vêm de `backend().desktop_name(i)?` quando presentes.

- [ ] **Step 4: Turn workspace creation into no-ops**

`new_workspace`, `ensure_workspaces_for_monitor` e `ensure_named_workspaces_for_monitor` apenas `tracing::warn!("desktops are managed by the task view")` e retornam `Ok(())`.

- [ ] **Step 5: Hide the unusable CLI command**

`swap-workspaces-with-monitor` recebe `#[clap(hide = true)]`; a mensagem enviada já é rejeitada pela Task 7.

- [ ] **Step 6: Run tests and build**

Run: `cargo test -p komorebi && cargo build --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/process_command.rs komorebi/src/monitor.rs komorebic/src/main.rs
git commit -m "feat(cli): reflect native desktops in workspace commands"
```

---

### Task 9: Remover a máquina de virtual desktop antiga

**Files:**
- Modify: `komorebi/src/lib.rs:287-346`, `komorebi/src/process_event.rs:200-261`, `komorebi/src/window_manager.rs:90`, `komorebi/src/state.rs:160`, `komorebi/src/static_config.rs:1340`, `komorebi-bar/src/widgets/komorebi.rs`

**Interfaces:**
- Consumes: `WindowManagerEvent::{DesktopChanged, DesktopCountChanged}` (Task 5).
- Produces: `NotificationEvent::Desktop(DesktopNotification)` no lugar de `VirtualDesktop`; `sync_desktop_count()` no início de `process_event`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn serialized_global_state_has_no_legacy_virtual_desktop_field() {
    let json = serde_json::to_string(&GlobalState::default()).unwrap();
    assert!(!json.contains("current_virtual_desktop_id"));
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p komorebi serialized_global_state`
Expected: FAIL.

- [ ] **Step 3: Delete the legacy machinery**

Remover `current_virtual_desktop()`, `CURRENT_VIRTUAL_DESKTOP`, `WindowManager.virtual_desktop_id`, `GlobalState.current_virtual_desktop_id`, `VirtualDesktopNotification`, e o bloco de `process_event` que retornava cedo fora do desktop associado. No lugar, `self.sync_desktop_count()?` no início de `process_event`.

- [ ] **Step 4: Update the bar**

`komorebi-bar` troca `VirtualDesktop` por `Desktop` e re-renderiza em `DesktopChanged`.

- [ ] **Step 5: Run tests and build**

Run: `cargo test -p komorebi && cargo build --workspace`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: remove registry-based virtual desktop tracking"
```

---

### Task 10: Filtrar cloak automático da troca de desktop

**Files:**
- Modify: `komorebi/src/process_event.rs` (braços `Cloak`/`Uncloak`, `uncloack_to_ignore`)
- Modify: `komorebi/src/window_manager.rs` (novo `desktop_transition_pending`)

**Interfaces:**
- Consumes: `DesktopBackend::current_desktop_idx`, `focused_desktop_idx`.
- Produces: `WindowManager::is_desktop_transition_cloak(&self, window: Window) -> bool`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn cloak_from_desktop_transition_does_not_trigger_reconciliation() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    fake.set_window_desktop(42, 0);
    // janela 42 vive no desktop 0; o foco vai para o desktop 1
    wm.focus_desktop(1).unwrap();
    assert!(wm.is_desktop_transition_cloak(Window::from(42)));
    HIDDEN_HWNDS.lock().clear();
    wm.process_event(WindowManagerEvent::Cloak(
        WinEvent::ObjectCloaked, Window::from(42))).unwrap();
    assert_eq!(wm.focused_desktop_idx, 1);
    assert!(!HIDDEN_HWNDS.lock().contains(&42));
}

#[test]
fn cloak_on_the_focused_desktop_is_still_processed() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    fake.set_window_desktop(43, 1);
    wm.focus_desktop(1).unwrap();
    assert!(!wm.is_desktop_transition_cloak(Window::from(43)));
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p komorebi cloak`
Expected: FAIL — método inexistente.

- [ ] **Step 3: Implement the filter**

`is_desktop_transition_cloak` devolve `true` quando o desktop nativo da janela (`backend().desktop_idx_for_window(hwnd)?`) difere de `focused_desktop_idx` — ou seja, o cloak veio do Windows escondendo outro desktop, não de uma ação do usuário. Os braços `Cloak`/`Uncloak` de `process_event` retornam cedo nesse caso, sem reconciliação, sem `hide`/`restore`, sem alterar `known_hwnds`. `uncloack_to_ignore` deixa de ser necessário e é removido junto com sua contagem. O cloak de uma janela do desktop focado continua caindo no fluxo atual.

- [ ] **Step 4: Run tests**

Run: `cargo test -p komorebi cloak`
Expected: PASS.

- [ ] **Step 5: Check by hand**

Manual: alternar 10× entre desktops com uma janela minimizada e uma em stack.
Expected: nenhuma janela sai do gerenciamento, nenhuma entra em `HIDDEN_HWNDS`.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/process_event.rs komorebi/src/window_manager.rs
git commit -m "fix(wm): ignore cloak events caused by native desktop transitions"
```

---

### Task 11: Redesenhar bordas após a troca

**Files:**
- Modify: `komorebi/src/window_manager.rs` (`focus_desktop`, `DesktopChanged`)
- Modify: `komorebi/src/border_manager/mod.rs`

**Interfaces:**
- Consumes: `focus_desktop`.
- Produces: `pub(crate) fn redraw_borders_for_focused_desktop(wm: &WindowManager)`; em testes, `pub(crate) static BORDER_REDRAWS: AtomicUsize`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn switching_desktop_redraws_borders_once() {
    let (mut wm, _fake) = setup_window_manager_with_fake_desktops(3);
    let before = border_manager::BORDER_REDRAWS.load(Ordering::SeqCst);
    wm.focus_desktop(1).unwrap();
    assert_eq!(border_manager::BORDER_REDRAWS.load(Ordering::SeqCst), before + 1);
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p komorebi switching_desktop_redraws_borders`
Expected: FAIL.

- [ ] **Step 3: Implement the redraw**

`redraw_borders_for_focused_desktop` destrói as bordas do desktop que saiu (`border_manager::destroy_all_borders`) e recria as do workspace focado de cada monitor com `win32`/`dwm`. Incrementa `BORDER_REDRAWS`. Chamado ao fim de `focus_desktop` e no braço `DesktopChanged`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p komorebi redraws_borders`
Expected: PASS.

- [ ] **Step 5: Check by hand**

Manual: trocar de desktop com bordas habilitadas.
Expected: bordas aparecem no desktop novo, nenhuma borda órfã fica no antigo.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/border_manager/mod.rs
git commit -m "fix(borders): redraw borders after native desktop switches"
```

---

### Task 12: Reaper e reconciliação com desktops invisíveis

**Files:**
- Modify: `komorebi/src/reaper.rs` (`watch_for_orphans`)
- Modify: `komorebi/src/monitor_reconciliator/mod.rs`
- Modify: `komorebi/src/window_manager.rs` (novo `is_window_on_other_desktop`)

**Interfaces:**
- Consumes: `DesktopBackend::desktop_idx_for_window`, `focused_desktop_idx`, `known_hwnds`.
- Produces: `WindowManager::is_window_on_other_desktop(&self, hwnd: isize) -> bool`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn window_on_other_desktop_is_not_reaped() {
    let (mut wm, fake) = setup_window_manager_with_fake_desktops(3);
    fake.set_window_desktop(99, 2);
    wm.focus_desktop(0).unwrap();
    assert!(wm.is_window_on_other_desktop(99));
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p komorebi window_on_other_desktop`
Expected: FAIL.

- [ ] **Step 3: Implement the guard**

`is_window_on_other_desktop` = `backend().desktop_idx_for_window(hwnd).ok().is_some_and(|i| i != self.focused_desktop_idx)`. `watch_for_orphans` ignora hwnds nessa condição antes de reportar órfão. `monitor_reconciliator` não reposiciona nem remove janelas de outros desktops.

- [ ] **Step 4: Run tests**

Run: `cargo test -p komorebi`
Expected: PASS.

- [ ] **Step 5: Check by hand**

Manual: deixar uma janela num desktop, trocar para outro, esperar 30s.
Expected: a janela continua gerenciada ao voltar.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/reaper.rs komorebi/src/monitor_reconciliator/mod.rs komorebi/src/window_manager.rs
git commit -m "fix(wm): never reap windows living on other desktops"
```

---

### Task 13: Parar de esconder janelas na troca de workspace

**Files:**
- Modify: `komorebi/src/monitor.rs:170` (`load_focused_workspace`), `komorebi/src/workspace.rs:389` (`hide`), `komorebi/src/window_manager.rs:1393` (`restore_all_windows`)

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
    HIDDEN_HWNDS.lock().clear();
    m.load_focused_workspace(false).unwrap();
    assert!(HIDDEN_HWNDS.lock().is_empty());
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p komorebi does_not_hide_other_workspaces`
Expected: FAIL (o loop atual empurra hwnds para `HIDDEN_HWNDS`).

- [ ] **Step 3: Stop hiding on workspace switch**

`load_focused_workspace` só chama `restore` no workspace focado; o loop que chama `workspace.hide(None)` sai. `Workspace::hide` permanece apenas para uso em stacks via `Container::load_focused_window`.

- [ ] **Step 4: Stop touching other desktops on shutdown**

`restore_all_windows` remove acentos, título e transparência apenas do workspace focado de cada monitor; janelas em outros desktops ficam como estão.

- [ ] **Step 5: Run tests, then check by hand**

Run: `cargo test -p komorebi`. Manual: 10 trocas de desktop seguidas com apps Electron e um stack.
Expected: PASS; nenhuma janela some, nenhuma borda órfã.

- [ ] **Step 6: Commit**

```bash
git add komorebi/src/monitor.rs komorebi/src/workspace.rs komorebi/src/window_manager.rs
git commit -m "refactor: stop hiding windows when switching workspaces"
```

---

### Task 14: Restauração de estado

**Files:**
- Modify: `komorebi/src/window_manager.rs:185` (`apply_state`), `komorebi/src/main.rs:318` (propagar o `Result`)

**Interfaces:**
- Consumes: `WindowManager::sync_desktop_count`.
- Produces: `WindowManager::apply_state(&mut self, state: State) -> eyre::Result<()>` (assinatura muda de `()` para `Result`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn applying_state_with_fewer_workspaces_than_desktops_keeps_all_desktops() {
    let (mut wm, _fake) = setup_window_manager_with_fake_desktops(3);
    let mut state = State::from(&wm);
    state.monitors.elements_mut()[0].workspaces.elements_mut().truncate(2);
    wm.apply_state(state).unwrap();
    assert_eq!(wm.monitors()[0].workspaces().len(), 3);
}

#[test]
fn applying_state_with_more_workspaces_than_desktops_does_not_lose_windows() {
    let (mut wm, _fake) = setup_window_manager_with_fake_desktops(2);
    let mut state = State::from(&wm);
    while state.monitors.elements()[0].workspaces().len() < 3 {
        state.monitors.elements_mut()[0].workspaces.elements_mut().push_back(Workspace::default());
    }
    wm.apply_state(state).unwrap();
    assert_eq!(wm.monitors()[0].workspaces().len(), 2);
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p komorebi applying_state`
Expected: FAIL.

- [ ] **Step 3: Reconcile before applying**

`apply_state` chama `sync_desktop_count()?` antes do loop, aplica `min(state_len, desktop_count)` workspaces por monitor, e para o excedente re-adota as janelas como flutuantes no desktop `desktop_count - 1` em vez de descartá-las. O early-return de contagem de monitores divergente continua igual. Atualizar a chamada em `main.rs` para `wm.lock().apply_state(state)?;`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p komorebi applying_state`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add komorebi/src/window_manager.rs komorebi/src/main.rs
git commit -m "fix(wm): reconcile restored state with native desktops"
```

---

### Task 15: Documentação e schema

**Files:**
- Modify: `README.md:67`, `docs/installation.md`, `docs/design.md`, `docs/usage/*.md`
- Regenerate: `schema.json`, `schema.bar.json`

**Interfaces:**
- Consumes: comportamento implementado nas tarefas 1–14.
- Produces: documentação e schema coerentes.

- [ ] **Step 1: Update the version floor**

`README.md`: "Windows 11 24H2 (build 26100.2605) and above", com nota de que Windows 10 não é mais suportado.

- [ ] **Step 2: Document the model**

Em `docs/design.md` e nos docs de uso: lista global de desktops, sem workspaces por monitor, `new-workspace`/`ensure-workspaces` como no-op, janela nova fica no desktop de origem, `swap-workspaces-with-monitor` removido.

- [ ] **Step 3: Regenerate the schemas**

Rodar o alvo de geração de schema do repo e conferir que `schema.json`/`schema.bar.json` só mudam nos campos removidos (`current_virtual_desktop_id`, `virtual_desktop_id`).

- [ ] **Step 4: Commit**

```bash
git add README.md docs schema.json schema.bar.json
git commit -m "docs: document native desktop workspaces"
```

---

## Dependências

- 1 → 2 → 3 bloqueiam todas as outras.
- 4, 5, 6, 7 dependem de 3.
- 8, 9 dependem de 4.
- 10, 11, 12 dependem de 5.
- 13, 14 dependem de 6 e 10.
- 15 é a última.
