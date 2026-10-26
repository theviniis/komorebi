> Acompanhamento atual: [plano de conclusão da migração](plans/2026-10-25-native-desktop-migration-completion.md).
> Etapas 1–4 e validação automatizada da etapa 5 concluídas em 2026-10-25: WindowApi object-safe cobre foreground, monitor, elegibilidade, metadados, geometria, enumeração e visibilidade de stacks. `enum_window_handles` retorna os HWNDs, e a descoberta os gerencia pelo desktop nativo real. O construtor público anterior e WindowsApi permanecem compatíveis.
> Suíte final: 183 aprovados, zero falhas e um ignorado histórico, em duas execuções; barra: cinco aprovados. Check do workspace, builds release dos três executáveis e geração/comparação de schemas passaram. Evidências e decisões constam no acompanhamento.
> O restante deste documento é histórico. O exemplo genérico `enum_windows<F>` não deve ser usado em `dyn WindowApi`; a implementação usa `enum_window_handles`. Contagens fixas, estimativas e checkboxes históricos abaixo não substituem os critérios vigentes. Validação manual e aceite final continuam pendentes.

# Plano: Trait `WindowApi` + Injeção de Dependência (Opção B)

---

## ✅ O que já está feito

| Item | Status |
|------|--------|
| Fix core: `window_desktops` tracking em `WindowManager` | ✅ Concluído |
| Fix: `desktop_idx_for_new_window` usa tracked state + fallback | ✅ Concluído |
| Fix: `is_desktop_transition_cloak` / `is_window_on_other_desktop` usam tracked state | ✅ Concluído |
| Debug logging removido de `WinvdBackend` | ✅ Concluído |
| Build release OK | ✅ Concluído |
| 3 testes críticos da correção WinVD passam | ✅ Concluído |

---

## 🎯 Próximos passos (Opção B)

### 1. Definir trait `WindowApi` em `windows_api.rs`
```rust
pub trait WindowApi: Send + Sync {
    fn foreground_window(&self) -> eyre::Result<isize>;
    // Outros métodos que WindowManager/Monitor usam:
    fn valid_hmonitors(&self) -> eyre::Result<Vec<(String, isize)>>;
    fn get_monitor_info(&self, hmonitor: isize) -> eyre::Result<MONITORINFOEXW>;
    fn enum_windows<F>(&self, callback: F) -> eyre::Result<()>
    where F: FnMut(isize) -> bool + Send;
    // ... adiante conforme uso
}
```

### 2. Duas implementações
```rust
// Real (produção)
pub struct RealWindowsApi;
impl WindowApi for RealWindowsApi { ... }

// Mock (testes)
#[cfg(test)]
pub struct MockWindowsApi {
    pub foreground_hwnd: Mutex<Option<isize>>,
}
impl WindowApi for MockWindowsApi {
    fn foreground_window(&self) -> eyre::Result<isize> {
        Ok(*self.foreground_hwnd.lock().unwrap().get_or_insert(12345))
    }
    // stubs para outros métodos
}
```

### 3. Injetar no `WindowManager`
```rust
// Em window_manager.rs
pub struct WindowManager {
    // ...
    window_api: Arc<dyn WindowApi>,
    // ...
}

impl WindowManager {
    pub fn new(window_api: Arc<dyn WindowApi>) -> Self { ... }
}
```

### 4. Atualizar `Monitor::move_container_to_workspace`
- Receber `window_api: &Arc<dyn WindowApi>` como parâmetro
- Ou acessar via `WindowManager` (já tem referência ao monitor)

### 5. Atualizar `setup_window_manager()` / `setup_window_manager_with_fake_desktops()`
```rust
fn setup_window_manager() -> (WindowManager, TestContext) {
    let window_api = Arc::new(MockWindowsApi::default());
    let wm = WindowManager::new(window_api);
    // ...
}
```

### 6. Ajustar `static_config.rs`
- Passar `Arc::new(RealWindowsApi)` na criação real

### 7. Verificar todos os usos de `WindowsApi::`
```bash
grep -r "WindowsApi::" komorebi/src/
```
Migrar cada chamada para `self.window_api.metodo()`

---

## 📋 Checklist de execução

| Passo | Arquivo(s) | Estimativa |
|-------|------------|------------|
| 1. Trait + RealWindowsApi + MockWindowsApi | `windows_api.rs` | 30 min |
| 2. Adicionar campo `window_api` no `WindowManager` | `window_manager.rs`, `static_config.rs` | 20 min |
| 3. Migrar chamadas `WindowsApi::` → `self.window_api.` | `window_manager.rs`, `monitor.rs`, `monitor_reconciliator.rs` | 40 min |
| 4. Atualizar `Monitor::move_container_to_workspace` | `monitor.rs` | 15 min |
| 5. Atualizar helpers de teste | `window_manager.rs` (test module) | 15 min |
| 6. Rodar testes + fix compilação | — | 20 min |

**Total estimado: ~2-2.5h**

---

## ⚠️ Riscos / Pontos de atenção

1. **`WindowsApi` tem muitos métodos estáticos** — migrar todos pode ser verboso. Priorizar só os que testes usam (`foreground_window`, `valid_hmonitors`, `enum_windows`).
2. **`Monitor` não tem `window_api` hoje** — precisa receber via parâmetro ou via `WindowManager`.
3. **`static_config.rs` cria `WindowManager`** — precisa instanciar `RealWindowsApi` lá.
4. **Compatibilidade** — manter `WindowsApi` struct pública para código externo (ex: `komorebic`, `komorebi-bar`) se eles usarem.

---

## ✅ Critério de aceite

```bash
cargo test -p komorebi -- --test-threads=1
# → 126 passed; 0 failed
```

Todos os 5 testes problemáticos passam **sem `#[ignore]`**.
