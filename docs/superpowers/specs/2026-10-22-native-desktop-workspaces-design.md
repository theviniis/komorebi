# Desktops virtuais nativos como workspaces

**Data:** 2026-10-22
**Status:** aprovado
**Implementação:** `docs/superpowers/plans/2026-10-22-native-desktop-workspaces.md`

## Contexto

O komorebi modela workspaces por conta própria: cada monitor tem seu
`Ring<Workspace>`, e trocar de workspace significa esconder todas as janelas do
workspace anterior (`Cloak`, `Minimize` ou `SW_HIDE` — ver
`Window::hide_with_border` em `komorebi/src/window.rs`) e restaurar as do novo
(`Monitor::load_focused_workspace`). Toda a complexidade de bordas órfãs,
apps Electron que reagem mal a cloak e "janelas que somem" vem dessa simulação.

O Windows já tem uma implementação nativa e acelerada por hardware do mesmo
conceito: as áreas de trabalho virtuais do Task View. O komorebi hoje só a
observa, lendo `CurrentVirtualDesktop` do registro
(`current_virtual_desktop()` em `komorebi/src/lib.rs`) para ignorar eventos
quando o usuário está num desktop que não é o "dele" — um paliativo que existe
justamente porque o modelo próprio não é o do Windows.

Esta especificação substitui a simulação pelo mecanismo nativo.

## Intenção

O usuário gerencia uma única lista de áreas de trabalho no Task View, e o
komorebi espelha essa lista: posiciona as janelas, troca de desktop usando o
Windows e deixa o DWM decidir o que está visível.

## Objetivos

- Uma única lista global de workspaces, compartilhada por todos os monitores,
  derivada dos desktops virtuais do Windows.
- Trocar de workspace é trocar de desktop nativo, sem esconder nem restaurar
  janelas.
- Mover container/janela entre workspaces é mover a janela de desktop nativo.
- Toda a configuração existente (layouts, padding, regras por workspace,
  nomes) continua funcionando, com `workspace_idx` significando índice de
  desktop.
- Win+Tab, Task View e Alt+Tab passam a refletir o estado real do komorebi.

## Não-objetivos

- Criar ou remover desktops virtuais. A lista é do usuário.
- Workspaces independentes por monitor.
- Suportar Windows abaixo de 11 24H2 (build 26100.2605).

## Anti-objetivos

Estas coisas tornariam o trabalho um fracasso mesmo que o código compilasse:

- Trazer para o desktop atual uma janela que nasceu em outro. O usuário pediu
  explicitamente o contrário: a janela fica onde nasceu.
- "Resgatar" janelas perdidas movendo-as silenciosamente.
- O komorebi virar dono da lista de desktops e apagar desktops com janelas
  dentro.
- Esconder janelas de workspaces não focados "por segurança". Se o DWM esconde,
  esconder de novo quebra o modelo.

## Restrições

- **Piso de versão:** build 26100.2605 (Windows 11 24H2) ou superior. Abaixo
  disso o processo aborta na inicialização com mensagem explícita. O README
  hoje promete "Windows 10 and above"; isso muda.
- **API não documentada:** enumerar, criar, trocar e mover desktops exige
  `IVirtualDesktopManagerInternal`, cujas assinaturas mudam entre builds. A
  única interface documentada (`IVirtualDesktopManager`) só informa em qual
  desktop uma janela está.
- **Um desktop por vez é visível**, e a lista é global — não há como um monitor
  estar no desktop 1 e outro no desktop 3.
- **Bordas do komorebi são janelas overlay** e pertencem ao desktop onde foram
  criadas.

## Abordagens consideradas

**A — Desktops como backend de troca, workspaces por monitor sincronizados.**
Mantém `Vec<Workspace>` por monitor, mas sincroniza o índice focado entre todos
eles e usa `switch_desktop` na troca. Menor refactor, compatível com toda a
configuração. Trade-off: o modelo de dados ainda *parece* ter workspaces
independentes por monitor, o que é uma mentira conceitual.

**B — Desktops substituem os workspaces (escolhida).**
Uma lista global é a fonte da verdade; o índice focado é único. Trade-off: mexe
no schema de estado, na barra, nas regras por workspace e na CLI, e remove
comandos que não têm equivalente.

**C — Espelhamento passivo.**
O komorebi só observa o desktop atual; o usuário troca com Win+Ctrl+seta.
Barato e sem risco, mas `focus-workspace` pelos keybinds deixaria de funcionar
como hoje — o que derrota o propósito de usar um tiling WM.

**Por que B:** o usuário escolheu explicitamente "igual para todos os
monitores", e o objetivo declarado é que o komorebi funcione *com* a função
nativa, não *ao lado* dela. A e C mantêm duas fontes de verdade.

**Dentro de B, uma decisão de escopo:** o modelo de dados continua com um
`Workspace` por monitor por desktop, em vez de uma única árvore global. Os
containers, layouts, padding e regras *são* inerentemente por monitor — dois
monitores no mesmo desktop precisam de dois layouts. O que se globaliza é o
comprimento dos anéis e o índice focado. Isso preserva as ~350 chamadas
existentes no formato `(monitor_idx, workspace_idx)` e reduz o blast radius
sem trair o modelo.

## Desenho

### Fonte da verdade

`WindowManager` ganha:

- `desktop_count: usize` — igual a `get_desktop_count()`;
- `focused_desktop_idx: usize` — o único índice de workspace focado.

Invariante: todos os monitores têm `workspaces().len() == desktop_count`, e
`monitor.focused_workspace_idx() == wm.focused_desktop_idx` para todo monitor.

`Monitor::focus_workspace` deixa de ser usado para trocar de workspace; passa a
existir só como ajuste interno de índice. `WindowManager::focused_workspace_idx()`
retorna o campo global em vez de perguntar ao monitor focado.

### Camada de acesso

Novo módulo `komorebi/src/virtual_desktop.rs`, única porta para o Windows:

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
```

A implementação real usa **`winvd` 0.0.49** (MIT), do mesmo autor cujo código
COM o komorebi já incorporou em `com/interfaces.rs`. Alternativa descartada:
escrever os `IVirtualDesktopManagerInternal` à mão — mais código nosso, não
testado, para o mesmo resultado. Custo aceito: uma segunda versão do crate
`windows` (0.58) na árvore de dependências.

A trait existe para permitir um backend falso nos testes unitários; o resto do
komorebi nunca importa `winvd`.

### Troca de workspace

```
focus-workspace N
  → valida N < desktop_count
  → backend.switch_desktop(N)
  → focused_desktop_idx = N
  → update_focused_workspace  (retile + bordas)
```

Nada de `hide`/`restore` de workspace. `HidingBehaviour` (`Hide`, `Minimize`,
`Cloak`) continua existindo, mas passa a valer apenas para **stacks** — várias
janelas no mesmo container, onde o komorebi ainda precisa escolher qual
mostrar.

### Eventos do Windows

Uma thread dedicada (`desktop_listener.rs`) consome `winvd::DesktopEvent` e
injeta no canal que já alimenta `process_event`:

| Evento nativo | `WindowManagerEvent` | Efeito |
|---|---|---|
| `DesktopChanged` | `DesktopChanged { new, old }` | atualiza índice, retila, redesenha bordas |
| `DesktopCreated`/`DesktopDestroyed` | `DesktopCountChanged` | `sync_desktop_count` |
| `DesktopMoved` | `DesktopMoved { desktop, old, new }` | permuta os workspaces correspondentes em todos os monitores |
| `DesktopNameChanged` | — | atualiza nomes exibidos na barra |

Troca feita pelo usuário (Win+Ctrl+seta, Task View) chega pelo mesmo caminho e
não deve disparar um `switch_desktop` de volta — senão há loop.

### Janela nova

Ao gerenciar uma janela, o desktop nativo dela é consultado:

- **mesmo desktop focado** → fluxo atual, entra no workspace focado e recebe
  foco;
- **outro desktop** → entra no workspace daquele índice, é posicionada pelo
  layout, **sem** receber foco e **sem** mudar `focused_desktop_idx`.

Regras de `workspace_matching_rules` continuam valendo e podem redirecionar a
janela depois, aí via `move_window_to_desktop`.

### CLI

| Comando | Novo comportamento |
|---|---|
| `focus-workspace N` | troca o desktop N; erro se não existir |
| `cycle-focus-workspace` | cicla sobre `desktop_count` |
| `move-container-to-workspace N` | `move_window_to_desktop` para cada janela do container, incluindo stack e flutuantes |
| `focus-workspace-numbers N` | alias de `focus-workspace N` (não há mais por-monitor) |
| `new-workspace` | no-op com warn: desktops são criados no Task View |
| `ensure-workspaces` / `ensure-named-workspaces` | no-op com warn |
| `workspace-name M W NAME` | grava o nome no desktop nativo; aparece no Task View |
| `swap-workspaces-with-monitor` | erro explicativo (não existe equivalente) |

### Remoções

Saem do código, porque o motivo deles desaparece:

- `virtual_desktop_id` (`WindowManager`) e `current_virtual_desktop_id`
  (`GlobalState`);
- `current_virtual_desktop()` e `CURRENT_VIRTUAL_DESKTOP`;
- `VirtualDesktopNotification::{Entered,Left}AssociatedVirtualDesktop` e o
  bloco de `process_event` que ignora eventos fora do desktop do komorebi;
- `Monitor::focus_workspace` como caminho de troca e `Workspace::hide` como
  caminho de troca.

Substituídos por `sync_desktop_count` no início de `process_event` e pelas
notificações de `DesktopChanged` para a barra.

### Piso de versão

`komorebi/src/version_gate.rs` lê `CurrentBuildNumber` e `UBR` de
`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` e aborta com mensagem
citando o build detectado e o mínimo exigido. Chamado como primeira coisa em
`main()`, antes de qualquer inicialização de COM.

### Restauração de estado

`komorebi.state.json` pode ter sido gravado com outra quantidade de desktops.

- estado com menos workspaces que desktops → aplica os que existem, cria os
  demais vazios;
- estado com mais workspaces que desktops → descarta o excedente sem perder
  janelas (elas voltam como flutuantes no último desktop válido, ou são
  re-gerenciadas pelo fluxo normal).

## O que fica para quem implementa

- Ordem e granularidade das tarefas: ver o plano.
- Nomes internos, organização de módulos e onde exatamente inserir os novos
  braços de `match`.
- Estratégia de filtro para o cloak automático (ver risco 1) — precisa ser
  decidida com o código na frente, testando contra apps reais.
- Redesenho de bordas: se basta destruir e recriar no desktop novo, ou se dá
  para reaproveitar as janelas existentes.

## Riscos

1. **Cloak automático.** Ao trocar de desktop, o Windows cloa as janelas do
   desktop que sai e descloa as do que entra, gerando WinEvents que o komorebi
   hoje interpreta como ação do usuário (`uncloack_to_ignore`, `HIDDEN_HWNDS`).
   Sem um filtro confiável, janelas somem do gerenciamento. É o risco mais
   provável de morder.
2. **Bordas overlay** presas ao desktop de origem. Redesenho a cada troca é
   obrigatório, e há um TODO antigo no código sobre bordas não redesenhadas ao
   voltar de outro desktop.
3. **Reconciliação.** `monitor_reconciliator` e `reaper` assumem que uma janela
   gerenciada está no desktop visível. Janelas em desktops não visíveis não
   podem ser tratadas como órfãs.
4. **API não documentada.** Uma atualização do Windows pode quebrar tudo.
   `winvd` fica fixado; a falha é explícita, não silenciosa.
5. **Clientes antigos.** Scripts que enviam `new-workspace` esperando novos
   workspaces passam a receber no-op com warn. É intencional, mas precisa estar
   documentado.

## Estratégia de teste

- **Unitário, com backend falso:** sincronização de contagem, índice global,
  `focus_desktop` afetando todos os monitores, janela nova em desktop não
  focado sem receber foco, `move_container_to_workspace` movendo todos os hwnds,
  nomes propagados, reconciliação de estado com contagem divergente.
- **Manual, na máquina do usuário (build 26300):** troca por keybind, troca por
  Win+Ctrl+seta, criar e remover desktop no Task View, mover janela entre
  desktops, abrir app em desktop não focado, 10 trocas seguidas verificando
  janelas e bordas.
