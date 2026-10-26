# Conclusão da migração para desktops nativos do Windows

Data: 2026-10-25

Status: etapas 1–4 concluídas; validação automatizada da etapa 5 concluída; validação manual e aceite final pendentes.

Worktree: `C:\Users\thevi\.local\share\opencode\worktree\dd874b\native-desktop-workspaces`

## Objetivo e decisões

Estabilizar a suíte serial e concluir a integração entre komorebi e os desktops nativos, incluindo movimentos realizados pelo Task View. Preservar as alterações locais existentes.

Os desktops têm identidade e foco globais. Cada monitor mantém seu workspace por desktop para containers, layouts e padding. Os comandos públicos continuam usando índices; internamente, a identidade estável deve preservar associações após remoções e reordenações.

Referências: `docs/PLAN_WINDOW_API_TRAIT.md`, `docs/superpowers/specs/2026-10-22-native-desktop-workspaces-design.md` e `docs/superpowers/plans/2026-10-22-native-desktop-workspaces.md`. Este documento acompanha a execução restante e substitui a contagem fixa de testes como critério de conclusão.

## Processo obrigatório de acompanhamento

- [x] Executar as etapas abaixo na ordem indicada, preservando mudanças preexistentes.

- [x] Em cada implementação, atualizar este documento com itens concluídos, testes executados, resultados e pendências.

- [x] Marcar um item somente quando implementado e validado; compilação sozinha não comprova comportamento.

- [x] Registrar mudanças de escopo ou abordagem na seção de decisões antes de considerá-las concluídas.

- [x] Manter critérios manuais pendentes até existir evidência de execução; não inferir aprovação a partir de testes unitários.

- [x] Concluir a migração somente quando todos os critérios de aceite estiverem marcados. Falhas, verificações bloqueadas ou evidências ausentes impedem a conclusão.

Cada registro deve conter data, etapa, alteração, comando ou cenário executado, resultado e pendências. Não registrar resultados esperados como resultados observados.

## Estado inicial observado

Execução anterior de `cargo test -p komorebi -- --test-threads=1`: **122 passaram, 3 falharam, 1 ignorado**.

Falhas: `monitor_reconciliator::tests::test_send_notification`, `state::tests::serialized_global_state_has_no_legacy_virtual_desktop_field` e `window_manager::tests::move_container_moves_every_window_to_target_desktop`.

Há consumidor em segundo plano do canal global do reconciliador, teste alterando `USERPROFILE` sem restauração e chamada real de foreground no caminho de movimentação. A influência da alteração de ambiente na falha de diretório de dados deve ser confirmada durante a correção.

O único teste ignorado identificado é `static_config::tests::backwards_compat`, por dependência de rede/rate limiting já documentada. Ele permanece como exceção explícita, fora do aceite desta migração. Não adicionar novos testes ignorados. Execução paralela da suíte fica fora deste ciclo.

## Etapa 1 — Estabilizar a infraestrutura de testes

- [x] Reproduzir as três falhas, isoladamente e na suíte serial, registrando diferenças.

- [x] Introduzir `WindowApi: Send + Sync`, implementação real delegando a `WindowsApi` e fake configurável.

- [x] Injetar `Arc<dyn WindowApi>` no `WindowManager`, mantendo o construtor público atual e adicionando construção com dependências explícitas.

- [x] Migrar as chamadas necessárias aos testes de movimentação e eventos e passar a dependência aos caminhos de `Monitor` envolvidos; evitar migração indiscriminada dos métodos estáticos.

- [x] Garantir compatibilidade com `dyn`: callbacks por referência a `dyn FnMut`, sem métodos genéricos na trait.

- [x] Preservar `WindowsApi` para os clientes existentes.

- [x] Fazer o loop do reconciliador receber canal explícito; usar canais próprios nos testes e encerrar/juntar as threads ao terminar.

- [x] Restaurar variáveis de ambiente modificadas pelos testes, inclusive após panic, especialmente `USERPROFILE`.

- [x] Fazer as fixtures manterem vivos contexto temporário e guard do backend; restaurar backend e globais modificados ao terminar.

- [x] Corrigir as três falhas sem depender de HWNDs reais, foreground do usuário ou ordem dos testes.

Aceite da etapa: cada teste antes falho passa isoladamente e a suíte serial completa passa sem novas exclusões ou dependência de sessão gráfica interativa para os testes desta migração.

## Etapa 2 — Corrigir movimentação e gerenciamento de janelas

- [x] Resolver uma única vez o alvo do comando: flutuante ativa ou container focado, incluindo todas as janelas da stack.

- [x] Validar destino antes de efeitos externos; atualizar desktop nativo e associação interna para `follow=false` e `follow=true`.

- [x] Trocar foco apenas depois da atualização das associações internas.

- [x] Em falha parcial, consultar o estado real das janelas afetadas, reconciliar associações e retornar erro explícito.

- [x] Tornar `Show`, `Manage` e `Uncloak` idempotentes, sem duplicar janelas já gerenciadas.

- [x] Identificar o monitor correspondente, aplicar regras e atualizar o layout do destino sem roubar foco de outro desktop.

- [x] Atualizar caches também nos caminhos com retorno antecipado.

- [x] Encaminhar `WindowChanged(hwnd)` para reconciliar movimentos externos sem mover a janela de volta.

- [x] Tratar o tracking como cache reconciliável, invalidando associações obsoletas. Não considerar que fallback por erro corrige consultas bem-sucedidas com dados antigos.

Aceite da etapa: movimentos internos e externos mantêm uma única associação por janela, Windows e komorebi concordam sobre o destino, e `follow` controla somente o acompanhamento do foco.

## Etapa 3 — Sincronizar identidade e ciclo de vida dos desktops

- [x] Acrescentar ao backend listagem ordenada com identificador estável, nome e identidade do desktop atual; implementar também no fake.

- [x] Manter interfaces públicas por índice e converter para identidade internamente.

- [x] Reconciliar criação, remoção e reordenação por identidade, preservando layouts dos desktops sobreviventes em todos os monitores.

- [x] Na remoção, consultar o destino real das janelas e reinseri-las sem descartá-las ou movê-las silenciosamente.

- [x] Derivar o índice focado da identidade atual, inclusive quando outro desktop atravessar sua posição.

- [x] Reconciliar eventos repetidos ou atrasados com o estado atual, sem loops de troca.

- [x] Unificar a finalização de eventos nativos: caches, índices, layouts necessários e publicação aos assinantes.

- [x] Propagar nomes e remoção de nomes para workspaces, caches e barra.

- [x] Reconstruir bordas ao mudar de desktop e preservar janelas de desktops não visíveis no reaper e reconciliador.

Aceite da etapa: todos os monitores têm exatamente um workspace por desktop nativo e o mesmo índice focado; remoções e reordenações preservam identidade, janelas e configurações dos sobreviventes.

## Etapa 4 — Completar compatibilidade e integração

- [x] Auditar comandos, regras, restauração de estado e conexão de monitores para impedir criação/remoção independente de workspaces.

- [x] Remover caminhos que escondem janelas somente por troca de desktop, preservando ocultação necessária às stacks.

- [x] Validar CLI, barra e assinantes com o estado global e eventos externos.

- [x] Confirmar o piso de Windows previsto na especificação e falha explícita em versão incompatível.

- [x] Atualizar documentação e regenerar schemas afetados, verificando as diferenças geradas.

- [x] Atualizar o documento anterior de `WindowApi` para apontar para este acompanhamento e refletir o resultado implementado.

Aceite da etapa: não existem caminhos suportados que recriem workspaces independentes dos desktops nativos; clientes e documentação refletem o comportamento entregue.

## Etapa 5 — Validação automatizada e manual

### Automatizada

- [x] Cobrir movimentação com/sem acompanhamento, stacks, flutuantes, destino inválido e falha parcial.

- [x] Cobrir eventos repetidos, movimento pelo Task View, monitor correto e ausência de roubo de foco.

- [x] Cobrir criação e remoção do primeiro, intermediário, último e atual desktop.

- [x] Cobrir reordenação nos dois sentidos com foco dentro e fora do intervalo afetado.

- [x] Verificar preservação de janelas, layouts, nomes, caches, notificações e invariantes de todos os monitores.

- [x] Cobrir restauração com contagem divergente e conexão/reconexão de monitor.

- [x] Executar `cargo test -p komorebi -- --test-threads=1` com zero falhas.

- [x] Repetir a suíte serial em novo processo para verificar vazamentos entre execuções.

- [x] Executar `cargo check --workspace` e `cargo build --release -p komorebi -p komorebic -p komorebi-bar`, registrando os resultados.

### Manual no Windows suportado

Registrar build do Windows, quantidade de monitores e resultado de cada cenário.

- [x] Trocar desktops por comandos e por Win+Ctrl+seta, verificando foco, barra e bordas.

- [x] Criar, remover, renomear e reordenar desktops pelo Task View, incluindo remoção intermediária e do desktop atual.

- [x] Mover janelas pelo Task View e pelo komorebi, incluindo stacks e flutuantes e acompanhamento ligado/desligado.

- [x] Gerenciar janela em desktop não focado sem mudar foco ou trazê-la ao atual.

- [x] Validar dois monitores e conexão/reconexão sem quebrar a quantidade ou foco dos workspaces.

- [ ] Reiniciar e restaurar estado com contagem de desktops diferente, sem perder janelas.

- [ ] Executar dez trocas consecutivas sem janelas duplicadas, perdidas, escondidas indevidamente ou deslocadas inesperadamente.

## Critérios finais de aceite — todos obrigatórios

- [ ] Todas as etapas de implementação e validação acima concluídas, com evidências no registro.

- [x] Suíte serial sem falhas e sem novos testes ignorados; exceção histórica de rede preservada e documentada.

- [x] Testes desta migração usam dependências falsas para efeitos Win32 e não alteram desktops/janelas reais.

- [x] Nenhuma thread de teste, variável de ambiente ou backend instalado contamina testes posteriores.

- [ ] Movimentação funciona com e sem acompanhamento, para stacks e flutuantes, inclusive em falha parcial.

- [ ] Task View e komorebi convergem sem duplicações, perda de gerenciamento ou movimentos de retorno indevidos.

- [ ] Criação, remoção e reordenação preservam identidades, layouts e janelas; foco e quantidade permanecem globais.

- [ ] Barra, assinantes e bordas refletem mudanças nativas; reaper e reconciliador preservam janelas não visíveis.

- [x] Compatibilidade, restauração, schemas, documentação, check do workspace e builds release validados.

- [ ] Todos os cenários manuais executados no Windows suportado, incluindo dois monitores.

- [ ] Nenhuma falha ou pendência de aceite aberta; documento atualizado para concluído somente após esta verificação.

## Registro de execução e decisões

### 2026-10-25 — Planejamento

- Documento criado antes da implementação, conforme solicitado.

- Decisões confirmadas: suíte serial estável; movimentos pelo Task View incluídos.

- Resultado de testes informado no estado inicial pertence à revisão anterior; nenhuma nova execução de testes foi realizada para criar este documento.

- Próxima ação: iniciar a etapa 1 e registrar a reprodução das falhas.

### 2026-10-25 — Etapa 1: isolamento e dependências

- Reprodução inicial nesta implementação: `cargo test -p komorebi -- --test-threads=1` resultou em **123 passaram, 2 falharam, 1 ignorado**. Falharam serialização do estado e movimento de stack; o canal passou nesta execução. A reprodução isolada anterior à correção não foi executada; o primeiro item da etapa permanece aberto por essa lacuna de evidência.

- Introduzida trait `WindowApi` object-safe, com implementações real e fake, injetada por `WindowManager::new_with_window_api`. O construtor anterior permanece disponível. O escopo inicial inclui foreground na validação de comando e no movimento do monitor; métodos de enumeração e outras chamadas Win32 serão acrescentados somente conforme a necessidade dos próximos cenários.

- O caminho do monitor recebe a dependência explícita; o método público anterior delega para a implementação real. `WindowsApi` permanece disponível aos demais clientes.

- O loop do reconciliador aceita receiver explícito. O teste usa canal privado, desconecta o sender e faz join da thread; não cria janela oculta nem deixa consumidor concorrendo pelo canal global.

- Guard de ambiente restaura `VAR` e `USERPROFILE` em Drop, inclusive no unwind. Após remover o vazamento de `USERPROFILE`, a serialização passou na suíte completa, confirmando a contaminação observada.

- A fixture de desktops retorna contexto que mantém socket e guard de backend vivos até o fim do teste e restaura o backend anterior.

- Validação após as correções: duas execuções de `cargo test -p komorebi -- --test-threads=1`, ambas com **125 passaram, 0 falharam, 1 ignorado**. O ignorado é a exceção histórica de rede. Nenhuma exclusão adicionada.

- `cargo fmt --all` executado; mudanças de formatação em arquivos alheios ao escopo foram retiradas. Alterações locais anteriores em `static_config.rs` e `window_manager.rs` preservadas.

- Pendências da migração: etapas 2–5 e todos os critérios manuais. Esta execução não comprova integração com Task View nem encerra a migração.

- Validação isolada após a correção: os três testes antes problemáticos passaram individualmente, com `cargo test -p komorebi --lib <nome completo> -- --exact --test-threads=1` (1 passou por execução).


### 2026-10-25 — Etapa 2: movimentos e eventos de janelas

- Novos testes reproduziram, antes da correção, três falhas em `cargo test -p komorebi --lib desktop_move_ -- --test-threads=1`: stack não transferida internamente com acompanhamento, chamadas nativas desnecessárias para o desktop atual e seleção divergente da flutuante entre o backend e o monitor.
- Comando agora captura foreground/alvo uma única vez, valida índice e workspace antes dos movimentos, transfere flutuante ou stack completa e atualiza caches antes de acompanhar o foco. Desktop atual é no-op; maximizada continua explicitamente não suportada, com rejeição anterior aos efeitos nativos.
- Falhas de backend consultam todos os membros, inclusive o membro que retornou erro e os ainda não tentados. A reconciliação preserva janelas e flutuantes; falhas de consulta são reportadas sem descartar a associação existente. O fake permite erro antes e depois do movimento nativo.
- `WindowChanged(hwnd)` agora atravessa backend, listener e `WindowManagerEvent::WindowDesktopChanged`. Consulta o destino nativo e atualiza associação, caches e layouts sem chamadas de movimento ou troca de desktop. Eventos repetidos são idempotentes e eventos sem janela gerenciada são ignorados.
- Destacamento de uma janela da stack durante reconciliação não usa `Workspace::remove_window`, que focava/restaurava janelas como efeito colateral. Atualização de layout por par monitor/desktop evita selecionar ou levantar janelas no desktop ativo. A troca nativa deixa a seleção de foreground ao Windows.
- Gerenciamento de janela em desktop não focado ocorre antes da mudança de foco do monitor. O monitor vem da janela; `Show`, `Manage` e `Uncloak` repetidos não acrescentam containers. Há aplicação do comportamento Create/Append, float override e regras de flutuação; regras de associação a outro workspace ainda precisam da auditoria abaixo.
- `WindowApi` ampliada somente com elegibilidade para gerenciamento e identificação do monitor, permitindo testar os caminhos de eventos com HWNDs fictícios.
- Suíte serial intermediária: **134 passaram, 0 falharam, 1 ignorado**; após casos adicionais: **138 passaram, 0 falharam, 1 ignorado**. `cargo check --workspace` passou, incluindo barra, cliente e GUI. Permanecem quatro warnings preexistentes no pacote komorebi; nenhum teste ignorado adicionado.
- `git diff --check` passou. Formatação limitada aos arquivos envolvidos; mudanças locais anteriores preservadas.
- **Etapa 2 ainda não concluída:** auditar/aplicar regras de associação de monitor/workspace de forma coerente com o backend nativo, cobrir o fluxo completo no desktop focado e finalizar caches/notificações dos outros retornos antecipados (eventos de ciclo de vida serão unificados na etapa 3). Identidade estável, schemas do novo evento, build release e cenários manuais continuam pendentes.

- Validação final deste bloco, incluindo erro retornado após o movimento nativo: `cargo test -p komorebi -- --test-threads=1` passou com **139 passaram, 0 falharam, 1 ignorado**. O comentário do tracking agora descreve cache reconciliável e distingue falha de consulta de resposta bem-sucedida obsoleta.

### 2026-10-25 — Correção do acompanhamento

- Conferidos os itens da etapa 1 com o código e os resultados já registrados. Os itens de migração das dependências necessárias, restauração de ambiente e correção das três falhas estavam indevidamente desmarcados; corrigido o acompanhamento, sem alterar implementação ou executar nova suíte.
- O item de reprodução prévia das três falhas isoladamente permanece pendente por falta dessa evidência histórica. A reprodução serial e a validação isolada posterior foram realizadas, conforme registros anteriores; isso não equivale à reprodução isolada antes da correção.
- Os três itens abertos da etapa 2 são pendências reais: cobertura completa de Show/Manage/Uncloak, regras de associação a monitor/workspace e caches de todos os retornos antecipados. Os cenários implementados em desktops não focados não encerram esses itens mais amplos.


### 2026-10-25 — Encerramento das pendências das etapas 1 e 2

**Evidências detalhadas:** [guia e limitações da reconstrução](evidence/README.md), [manifesto](evidence/stage1-reconstruction.json), [patch da reconstrução](evidence/stage1-reconstruction.patch) e [log de validação atual](evidence/stages1-2-validation.log).

#### Etapa 1 — investigação e isolamento encerrados

- Recuperado o log original preexistente com **122 passaram, 3 falharam, 1 ignorado**, incluindo o receiver vazio do reconciliador. A cópia está em `evidence/stage1-historical-suite.log`; o original não foi alterado.
- Exportado o commit-base em diretório temporário separado e reconstruídos os ajustes locais conhecidos de tracking. O estado local anterior não tinha snapshot completo: a reconstrução é diagnóstica e não uma alegação de identidade byte a byte com o checkout antigo. Base e hashes estão no manifesto. O patch foi aplicado em uma cópia limpa da base e os três hashes normalizados conferiram.
- Os três testes antigos passaram isoladamente na reconstrução; a suíte serial da mesma reconstrução falhou em serialização e movimento (**123 passaram, 2 falharam, 1 ignorado**). Essas diferenças, antes não investigadas, agora estão registradas no log e na tabela de evidências. Não se pressupõe que uma falha de contexto deva aparecer em toda execução isolada.
- Reproduzida isoladamente a falha de diretório com USERPROFILE contaminado, executando diretamente o binário de teste em processo separado. Reproduzida de modo controlado a condição do canal vazio com um segundo receiver retirando a mensagem. A falha do movimento permaneceu observável na suíte anterior e não na execução isolada; ela dependia do acesso real ao foreground, removido dos testes atuais. A investigação fecha a lacuna histórica sem fabricar uma reprodução exata inexistente.
- Adicionados testes que comprovam restauração de USERPROFILE e de backend/caches/regras após panic. A fixture também restaura o cache do reaper, a lista de HWNDs ocultos e o contador de bordas. O listener privado é exercitado enquanto 20 notificações são enviadas e recebidas pelo canal global; sua thread termina com disconnect e join.
- A repetição da suíte revelou dois testes legados de Monitor ainda usando foreground real (Win32 87). Ambos agora recebem FakeWindowApi; passaram isoladamente e nas repetições completas. A falha intermediária foi preservada no log, não contada como aprovação.

#### Etapa 2 — implementação e regressões encerradas

- Show, Manage e Uncloak usam o mesmo caminho para desktops focados e não focados. Há cobertura das seis ordens possíveis repetidas nove vezes, para tiled e flutuantes, mantendo uma única associação. Eventos de janelas já existentes preservam stacks; só refletem a seleção de foreground já feita pelo Windows no desktop atual.
- Regras agora validam monitor e desktop antes dos efeitos; não criam workspaces nativos inexistentes nem escondem a janela para simular mudança de desktop. Movem pelo backend nativo e atualizam associação, geometria e layouts dos dois destinos envolvidos. O último matching elegível prevalece; comportamento Append e flutuantes são preservados no destino.
- Regra initial_only só é marcada após sucesso, permite retry após falha e não desfaz um movimento posterior pelo Task View. Falhas após movimento nativo ou na geometria do monitor reconciliam o desktop real sem perder a janela; há testes para esses casos e para destinos inválidos.
- A finalização de process_event é centralizada: todos os retornos do processamento, inclusive inelegibilidade e erros após mutação parcial, passam pela atualização de known_hwnds, window_desktops e cache do reaper. Quando há estado alterado, ele é publicado mesmo se o processamento retornar erro; o erro original é preservado.
- Testes com assinante por socket verificam a publicação após movimento por regra no retorno antecipado de MouseCapture e após erro de centralização de flutuante já adicionada. O servidor do teste tem timeout, sinal de parada e join.
- A fronteira WindowApi cobre metadados para regras, geometria e visibilidade de stacks usados nesses caminhos. Inclusão em stack tem operação de bookkeeping separada; o fake e a preparação das fixtures não chamam APIs reais de ocultação para HWNDs fictícios. O backend real preserva o comportamento de stacks.
- Removido o teste antigo que somente montava o outline de uma operação já substituída. Ele foi substituído pelos testes de comportamento de regras e de eventos; não houve adição de testes ignorados.

#### Resultado final desta execução

- `cargo test -p komorebi -- --test-threads=1`, em dois processos novos após a última alteração: **153 passaram, 0 falharam, 1 ignorado**, em ambas as execuções. Binário principal e doc-tests também passaram.
- Os três testes originalmente problemáticos passaram individualmente na versão final, com `cargo test -p komorebi --lib <nome completo> -- --exact --test-threads=1`.
- `cargo check --workspace`: **passou**, incluindo cliente, GUI e barra. Os quatro warnings já existentes no pacote komorebi permanecem fora deste escopo.
- `git diff --check`: **passou**. Alterações locais anteriores preservadas.
- Os checkboxes das etapas 1 e 2 estão encerrados com as evidências acima. A migração inteira não está concluída: identidade/ciclo de vida (etapa 3), compatibilidade/schemas (etapa 4), build release e todos os cenários manuais (etapa 5) continuam pendentes. Nenhuma validação manual do Windows foi inferida dos eventos simulados.


### 2026-10-25 — Etapa 3: identidade e ciclo de vida concluídos

**Evidência de validação:** [stage3-validation.log](evidence/stage3-validation.log). As execuções intermediárias e as duas execuções finais estão preservadas nesse arquivo.

#### Implementação e decisões

- Backend real e fake agora oferecem snapshot ordenado de IDs estáveis, nomes e identidade atual. O backend real usa GUIDs do winvd, confere lista/nomes/foco novamente e rejeita leituras inconsistentes; o fake modela criação, remoção e reordenação preservando as identidades das janelas.
- O WindowManager guarda a ordem nativa de IDs. Cada Workspace também guarda sua identidade em memória, preservada ao clonar um monitor para cache; esse campo é omitido da serialização pública. Comandos continuam recebendo índices, sincronizam o snapshot e convertem o destino para ID. Trocas e movimentos reais usam GUIDs; uma reordenação entre duas chamadas de movimento não muda o desktop de destino da stack.
- sync_desktop_count agora reconcilia workspaces por identidade em todos os monitores, em vez de simplesmente truncar a lista. Configurações, containers e layouts dos sobreviventes são preservados. A associação de um monitor em cache usa os IDs que ele próprio guardou, mesmo quando a ordem global mudou durante sua ausência.
- Na remoção, todos os destinos das janelas são consultados por ID antes da alteração do estado. Uma falha para janela viva retorna erro e mantém o último estado coerente, para retry na próxima sincronização; não há fallback silencioso para outro desktop nem descarte. HWND confirmado inválido via WindowApi não bloqueia a remoção. Quando há janelas a adotar, o snapshot é conferido novamente antes do commit.
- Stacks são preservadas quando todos os membros foram para o mesmo desktop. Se o Windows os separou, são criados containers distintos, com IDs distintos, preservando os membros e o estado de lock. Janelas flutuantes continuam flutuantes. Containers de monocle de desktop removido entram como containers normais no destino; maximizadas entram na coleção flutuante sem chamada de unmaximize, preservando as janelas e a configuração do desktop sobrevivente.
- Eventos Changed/CountChanged/Moved/Renamed são sinais para consultar o estado atual, não instruções para reaplicar posições antigas. Eventos repetidos ou atrasados não provocam troca de desktop de retorno. O índice focado é derivado do ID atual. A tradução não descarta Changed/Renamed somente porque um objeto antigo perdeu seu índice.
- Nomes são reconstruídos também quando vazios, limpando workspaces, monitor.workspace_names e o cache global. A finalização comum publica o estado aos assinantes; testes por socket verificam nomes/foco recebidos. State detecta mudanças nos campos globais de foco/contagem mesmo sem monitores conectados.
- Bordas são reconstruídas quando foco/topologia mudam. Repetir o mesmo evento não provoca nova reconstrução. Layouts são atualizados sem selecionar outra janela ou desktop.
- Reaper valida novamente a associação de notificações antigas e protege janelas de desktop não visível, inclusive antes de o evento nativo chegar à fila do WM. O mesmo predicado de proteção é usado pelo reconciliador de monitores. Retirado o lock do cache antes de adquirir o lock do WM no polling do reaper, eliminando a inversão com a reconstrução dos caches.
- A fixture genérica de WindowManager agora também instala/restaura backend nativo fake. Isso inclui os testes legados de foco e nomes, evitando que eles alcancem o winvd real ao validar a suíte. Os testes de eventos foram ajustados para primeiro alterar o estado do fake Windows e então enviar a notificação correspondente.

#### Cenários comprovados

- Quinze testes de regressão de identidade no WindowManager, mais um teste independente do ciclo de vida do backend fake.
- Todas as reordenações distintas de quatro desktops e todas as posições de foco: **48 combinações**, com dois monitores, eventos repetidos e foco deliberadamente atrasado.
- Criação em todas as posições; remoção do primeiro, intermediários, último e atual; preservação de configurações e janelas nos dois monitores.
- Consulta falha com preservação do estado e retry; HWND fechado; snapshots vazios/duplicados/com foco inexistente rejeitados antes da mutação; stacks separadas pelo destino real de cada membro; monocle e maximizada de desktop removido.
- Limpeza de nomes, publicação por socket, foco/contagem sem monitores, reconstrução única das bordas, notificações obsoletas do reaper e proteção antes de eventos nativos pendentes.
- Reinserção de monitor guardado em cache após reordenação, mantendo sua associação original; o campo de ID interno não aparece no JSON público. Esse teste não substitui o cenário manual de hotplug.
- Topologia alterada antes de um comando por índice e reordenação no meio do movimento de stack, com e sem acompanhamento. Os membros permanecem no mesmo desktop por ID, embora seus índices mudem durante a operação.

#### Resultado final

- `cargo test -p komorebi -- --test-threads=1`, em dois processos novos após as últimas alterações: **169 passaram, 0 falharam, 1 ignorado**, em ambas as execuções. Binário principal e doc-tests passaram. Nenhum teste ignorado adicionado.
- `cargo check --workspace`: **passou**, incluindo barra, cliente e GUI. Restam três warnings já existentes; o warning de variável não usada no reaper foi removido ao corrigir seu fluxo de locks.
- `git diff --check`: **passou**. Mudanças locais anteriores preservadas.
- Todos os itens da etapa 3 marcados com base na implementação e nas evidências automatizadas. Próxima etapa: auditoria de compatibilidade, comandos, regras, restauração e conexão de monitores (etapa 4). Schemas, build release e cenários manuais continuam pendentes; a migração completa ainda não foi declarada concluída.


### 2026-10-25 — Etapa 4 em implementação

- Auditados comandos, caminhos de configuração, restauração, descoberta inicial e reconciliação de monitores. Encontrados CloseWorkspace removendo um workspace local, criação por configuração/Monitor, restauração que sobrescrevia janelas e foco e reconexão que minimizava/restaurava aplicações.
- Comandos legados de criação/remoção agora são no-op com aviso; operações sem equivalente entre monitores retornam erro explícito. APIs de Monitor não ampliam/removem workspaces por conta própria, e os testes antigos foram atualizados para preparar snapshots nativos explicitamente.
- Restauração reimplementada com preflight de destinos reais, preservação das janelas novas, modos, resize por container e configuração atual; sem troca de desktop. Preselection não é restaurada. Estado legado tem defaults para os campos globais. ApplyState propaga erros.
- Reconciliação física passou a usar snapshot completo de monitores, preferências e cache, sem minimizar aplicações nem trocar desktop. Associações atuais prevalecem sobre caches antigos. Query com falha retorna erro e restaura o estado/cache anteriores.
- Inicialização enumera HWNDs e passa pelo gerenciamento com desktop nativo, incluindo janelas cloaked por outro desktop. Nomes configurados usam backend nativo, com precedência determinística e sem criação de desktops excedentes.
- Foco/nome/ciclo passam pela sincronização global; EagerFocus troca o desktop nativo. Movimentos entre monitor e desktop usam o mesmo caminho por ID. A barra consome foco/contagem globais e trata snapshots incompletos. Comandos também publicam alterações parciais antes de retornar erro.
- Quatorze testes novos de compatibilidade passaram; dois testes novos da barra passaram. A primeira execução após vedar criação/remoção local encontrou 15 testes legados presos à semântica anterior; foram reescritos para validar os novos contratos, sem ignores. Uma suíte intermediária voltou a 169 aprovados/zero falhas/um ignorado antes dos testes adicionais.
- Guia de migração criado em `docs/native-desktop-workspaces.md`; ajuda da CLI e descrições de schema atualizadas. Schemas gerados, revisão das diferenças, suíte final, check e build release ainda não foram registrados como concluídos neste ponto.


### 2026-10-25 — Etapa 4 concluída e etapa 5 automatizada concluída

Evidências: [stage4-validation.log](evidence/stage4-validation.log) e [stage4-release-build.log](evidence/stage4-release-build.log).

- Suíte final em dois processos novos: **183 passaram, 0 falharam, 1 ignorado histórico**, nas duas execuções. Barra: **5 passaram, 0 falharam**. `cargo check --workspace` passou. Nenhum ignore foi adicionado; os testes nativos usam backend e WindowApi falsos.
- `cargo build --release -p komorebi -p komorebic -p komorebi-bar` passou. Hashes e tamanhos dos três executáveis estão no log de validação. Permanecem dois warnings legados do komorebi e o aviso de compatibilidade futura da dependência net2.
- A geração revelou estouro da stack até no `komorebic --help` debug. O build script reserva 8 MiB de stack para o executável no Windows (argumentos específicos de MSVC/GNU). Ajuda e geradores foram executados com sucesso após a correção; o cabeçalho PE confirma a reserva nos executáveis debug/release MSVC. GNU não foi compilado neste ambiente.
- Regenerados schemas de configuração estática, notificações, socket e barra; ASC foi comparado e permaneceu igual. Todos os cinco são JSON válido e todas as referências locais resolvem. Geradores release foram executados e comparados semanticamente com os artefatos debug, sem diferenças.
- Revisão dos schemas: configuração estática muda descrições (incluindo documentação anterior defasada). Barra incorpora defaults de `desktop_count`/`focused_desktop_idx` e deixa esses campos opcionais para estado legado, além das descrições. O schema anterior da barra continha logs de compilação em UTF-16 antes do JSON; a regeneração usa somente stdout e UTF-8. Novos schemas de socket/notificação preservam tags e argumentos; notificações incluem eventos nativos e foco/contagem globais, sem IDs internos de runtime.
- Atualizadas dez páginas de ajuda a partir da CLI executada, guia de migração e README. `just jsonschema` contempla também socket e notificações; `just` não está instalado neste ambiente, portanto os geradores correspondentes foram executados diretamente. O documento histórico de WindowApi aponta para o resultado atual e esclarece a enumeração object-safe implementada.
- A auditoria e os testes cobrem criação/remoção legadas como no-op, operações não suportadas com erro, regras, foco global, movimento entre monitores, restauração transacional e hotplug sem minimizar janelas nem trocar desktops. Isso encerra o escopo de implementação da etapa 4; não substitui a prova interativa do comportamento real.
- Pré-condições manuais consultadas sem alterar a sessão: **Windows build 26300.9711 (26H2)**, dois monitores lógicos: DISPLAY1 principal 3440×1440 em (0,0), DISPLAY2 1920×1080 em (724,-1080). Isso confirma ambiente elegível, não aprovação de nenhum cenário manual.
- **Pendência obrigatória:** executar e registrar todos os cenários da seção manual da etapa 5 com os binários desta implementação. O daemon não foi iniciado/substituído nesta validação. Critérios finais que dependem dessa execução continuam abertos. A migração inteira não foi declarada concluída.


### 2026-10-25 — Primeira rodada manual e correções

- Usuário informou que os cenários 1 e 2 funcionaram. Aprovação parcial: o teste de atalhos revelou depois navegação circular da primeira para a última área; esse cenário deve ser repetido após a correção. O cenário 7 pareceu correto, ainda sem confirmação detalhada das dez trocas e de todos os invariantes.
- Cenários 3 e 4 aguardam execução após esclarecimento: mover stacks/flutuantes com e sem acompanhamento; gerenciar janela de desktop em segundo plano sem alterar foco. Cenário 6 significa reiniciar o komorebi/restaurar estado, não reiniciar o PC.
- Cenário 5 **falhou**: secundário desativado pelo NVIDIA App, sem trazer suas janelas ao primário. O evento WM_DISPLAYCHANGE tratava apenas resolução; precisa consultar a topologia completa mesmo sem WM_DEVICECHANGE. Além disso, a adoção de janelas deve comparar a árvore atual, não known_hwnds potencialmente obsoleto após a remoção.
- Correções em implementação: navegação adjacente sem wrap, reconciliação completa no WM_DISPLAYCHANGE, deduplicação da adoção pela árvore atual e regressões específicas. Build/testes desta rodada e reteste manual ainda pendentes neste registro.
- O atalho local Ctrl+Win+esquerda chama script de usuário, que envia cycle-workspace previous pela CLI instalada. A tag do comando permanece compatível; a correção está no daemon, sem modificar atalhos pessoais.

- A suíte após as duas regressões passou com **185 aprovados, zero falhas, um ignorado histórico**. A navegação testa contagens 1 e 3, ambos os limites e movimentos internos. A regressão de hotplug reproduz cache antigo apontando ao monitor removido e preserva janela tiled e flutuante em desktops diferentes.
- Antes do build final, a instância do release anterior foi encontrada rodando. Stop pelas CLIs nova e instalada falhou com Windows 10022. O processo foi encerrado por PID/caminho verificados, conforme autorização anterior de parada, e restore-windows executou sem erro. **Causa do erro de socket ainda não estabelecida**; verificar comunicação e parada na próxima inicialização. Não atribuir automaticamente ao hotplug.
- Encontrada escrita do arquivo real komorebi.hwnd.json pela suíte; DATA_DIR de unit tests agora aponta para diretório temporário por processo, isolando arquivos e sockets da sessão real. Suíte e release finais devem ser verificados após esse ajuste.


#### Roteiro concreto dos cenários que precisaram de esclarecimento

- **3 — movimento:** com pelo menos dois desktops, enviar uma janela flutuante e depois uma stack com `send-to-workspace 1`: permanecer no desktop atual, encontrar todos os membros no destino. Repetir com `move-to-workspace 1`: acompanhar até o destino. Fazer também o movimento de uma janela pelo Task View e conferir que o komorebi aceita o destino sem trazê-la de volta. Uma stack é um container com várias janelas; validar que não fica parcialmente no desktop anterior.
- **4 — desktop não focado:** deixar uma aplicação no desktop 2, voltar ao desktop 1 e reiniciar o komorebi. A descoberta deve gerenciar a aplicação do desktop 2 sem tirar o foco do desktop 1. Ao visitar o desktop 2, a janela deve estar gerenciada, sem duplicação.
- **6 — restauração:** salvar o estado do komorebi, alterar a quantidade de desktops no Task View, reiniciar somente o komorebi e aplicar o estado salvo. Conferir janelas preservadas nos desktops nativos atuais e ausência de workspaces extras. Não requer reiniciar o Windows. É distinto de apenas reiniciar sem aplicar estado.
- **Reteste obrigatório de 1:** na primeira área, Ctrl+Win+esquerda deve permanecer nela; na última, Ctrl+Win+direita deve permanecer nela. No meio, cada atalho deve andar uma posição. Conferir barra/bordas. A validação nova testa o comando enviado pelo atalho, sem modificar o script pessoal.
- **Reteste obrigatório de 5:** desativar novamente o secundário pelo NVIDIA App, aguardar a tolerância de 3 segundos e conferir transferência das janelas ao primário, inclusive ao visitar os demais desktops. Reativar o monitor e conferir gerenciamento, contagem/foco globais e ausência de duplicações.


#### Resultado da correção após a rodada manual

- Suíte final isolada: **185 passaram, 0 falharam, 1 ignorado histórico**, em dois processos novos. Evidências: [primeira execução isolada](evidence/manual-round1-tests-isolated.log) e [repetição](evidence/manual-round1-tests-repeat.log).
- Build release final dos três executáveis passou: [log](evidence/manual-round1-release-final.log). Ajuda da CLI confirma navegação sem wrap e schemas estático/socket/notificação continuam semanticamente iguais aos artefatos entregues: [hashes e conferência](evidence/manual-round1-artifacts.log). `git diff --check` passou.
- Não foi necessário alterar o script pessoal do atalho. Correção entregue no mesmo diretório target/release. O daemon anterior foi encerrado; o novo não foi iniciado automaticamente. Erro 10022 de stop continua registrado para verificar após a próxima inicialização.
- Correções automatizadas concluídas; a falha manual do NVIDIA App e os limites dos atalhos continuam **aguardando reteste com o novo executável**. Não considerar o cenário 5 aprovado apenas porque a regressão passou. O aceite global continua aberto.


### 2026-10-25 — Confirmação manual do cenário 4

- Usuário confirmou **“4. Funcionou”** após o roteiro de deixar uma aplicação no desktop 2, voltar ao desktop 1, reiniciar o komorebi e conferir gerenciamento no desktop 2 sem mudança involuntária de foco durante a inicialização. Cenário 4 marcado como aprovado por validação manual.
- Cenário 3: usuário informou movimento bem-sucedido pelo atalho Alt+Shift+1. Janelas separadas no Task View são esperadas, pois o agrupamento em stack pertence ao komorebi. Cobertura de flutuantes e movimento com/sem acompanhamento continua pendente de confirmação.
- Cenário 6: usuário informou “Funcionou, aparentemente”; falta confirmar aplicação do estado salvo após alteração da quantidade de desktops. Aprovação preliminar não encerra esse checkbox.
- Retestes dos limites de navegação e da desativação do monitor pelo NVIDIA App, assim como comunicação/parada após erro 10022, permanecem sem confirmação nesta rodada. O aceite global continua aberto.


### 2026-10-25 — Reteste manual da desativação pelo NVIDIA App aprovado

- Após solicitação explícita para repetir a desativação do secundário no NVIDIA App e conferir as janelas no primário com o novo build, o usuário confirmou: **“Já fiz e funcionou”**. A falha de transferência ao desativar o monitor está aprovada no reteste manual.
- O checkbox composto de conexão/reconexão de dois monitores permanece pendente apenas da confirmação de reativação/reconexão, preservação da contagem/foco globais e ausência de duplicações. A confirmação recebida não foi ampliada para esses passos não relatados.
- Continuam pendentes as demais confirmações já registradas: limites de navegação, flutuantes/stacks com e sem acompanhamento, restauração efetiva do estado salvo após mudança de contagem, dez trocas com invariantes e comunicação/parada após erro 10022. Aceite global ainda aberto.


### 2026-10-25 — Reconexão manual do segundo monitor aprovada

- Usuário relatou que, ao reativar o monitor, as janelas piscaram/mudaram de tamanho durante o ajuste e depois se organizaram corretamente. As janelas permaneceram no destino atual, sem retorno automático ao monitor anterior; esse comportamento corresponde à preservação da associação atual implementada.
- Após pergunta sobre preservação do foco e ausência de duplicação/perda, respondeu **“Sim, nenhuma janela ficou duplicada ou perdida”**. Cenário composto de dois monitores/conexão/reconexão marcado como aprovado com base no relato manual, complementando a desativação pelo NVIDIA App já aprovada.
- A reorganização transitória foi observada, sem relato de instabilidade persistente após estabilização. Não foi registrada como falha bloqueadora nem omitida da evidência.
- Aceite global continua aberto para as confirmações restantes; esta aprovação não encerra restauração, demais modos de movimento, limites dos atalhos ou a investigação de comunicação/parada.


### 2026-10-25 — Limites de navegação aprovados e atalho nativo liberado

- Usuário confirmou que Ctrl+Win+esquerda permanece na primeira área e Ctrl+Win+direita permanece na última. Em conjunto com o relato anterior de trocas/foco/barra/bordas, cenário 1 marcado como aprovado.
- Novo relato: Ctrl+Win+setas não funcionava com komorebi fechado. Identificadas duas entradas pessoais no whkdrc que capturavam essas teclas e executavam komorebi-cycle-active-workspace.ps1, dependente da CLI/daemon. O whkd continuava ativo independentemente do komorebi.
- Comentadas somente essas duas entradas em C:\Users\thevi\.config\whkdrc, preservando os demais atalhos. Backup integral: whkdrc.before-native-arrows-20261025-235109.bak. Script pessoal preservado. Reiniciado apenas whkd com --config apontando ao arquivo; processo novo confirmado ativo, sem captura dessas combinações na configuração.
- Ctrl+Win+setas agora fica reservado ao Windows. O comando cycle-workspace mantém a correção de limites para outros atalhos/clientes. Não houve alteração de código ou necessidade de novo build nesta correção da configuração pessoal.
- Verificação interativa **com komorebi fechado** ainda aguarda confirmação; não foi inferida apenas da edição/releitura da configuração. Não foi encerrada a instância do komorebi nesta rodada.


### 2026-10-25 — Geração do instalador solicitada

- Usuário solicitou build do instalador. Seguir o empacotamento MSI existente em wix/main.wxs e no workflow Windows, sem instalar o produto na sessão.
- Compilar em release os seis executáveis incluídos no manifesto: komorebi, komorebic, komorebic-no-console, komorebi-gui, komorebi-bar e komorebi-shortcuts. Reutilizar a versão 0.1.42 do projeto, sem alteração de versão/código de upgrade.
- Preparadas ferramentas cargo-wix 0.3.9 e WiX portátil 3.14.1.8722, obtido do release oficial wix3141rtm. SHA256 do ZIP: 6ac824e1642d6f7277d0ed7ea09411a508f6116ba6fae0aa5f2c7daa2ff43d31. Ferramentas portáteis ficam em target/installer-tools.
- Processo: concluir release completo, gerar MSI x64 com o manifesto existente, inspecionar conteúdo/versão e comparar arquivos empacotados com os binários release; registrar resultado e hash. Geração do pacote não comprova instalação/upgrade nem encerra o aceite manual restante.


#### Instalador gerado e inspecionado

- [x] Build release dos seis componentes concluído: [log](evidence/installer-release-build.log).
- [x] MSI x64 versão 0.1.42 gerado com wix/main.wxs e cargo-wix, sem executar instalação: [log](evidence/installer-msi-build.log).
- [x] MSI decompilado e payload extraído com dark.exe; plataforma/versão conferidas e hashes dos seis executáveis comparados ao release. Licença idêntica ao arquivo do projeto: [inspeção](evidence/installer-msi-inspection.log), [hashes e tamanhos](evidence/installer-msi-artifacts.log).
- Arquivo entregue: `target/wix/komorebi-0.1.42-native-desktops-x86_64.msi`. SHA256: `b550ad13180e2294eb573db88ac4e97cc68d021c689aaa9227fff3f5e0fef958`.
- Comando de empacotamento: `cargo wix --no-build -p komorebi --nocapture -I .\wix\main.wxs --bin-path .\target\installer-tools\wix-3.14.1 --target-bin-dir .\target\release --output .\target\wix\komorebi-0.1.42-native-desktops-x86_64.msi`.
- Instalador apenas gerado e inspecionado; instalação/upgrade não executados. Demais pendências de aceite manual continuam vigentes.


### 2026-10-26 — Tema/borda ativa: bloqueio de inicialização dos consumidores

- Usuário informou que a borda ativa não respeita o tema. Inspeção confirmou daemon instalado em Program Files, leitura de C:\Users\thevi\komorebi.json, border=true, largura 4, offset -1, estilo System e tema Custom. Borda Single aponta a Base0D=#C53788; stacks/flutuantes/monocle usam outras cores explicitamente configuradas.
- Logs após inicialização registram continuamente canais cheios de border_manager/transparency_manager. Ambos os consumidores faziam send bloqueante da atualização inicial antes do primeiro receive. A descoberta de janelas agora pode preencher essas filas antes de iniciar os workers, causando autobloqueio mesmo com o restante do daemon ativo.
- Correção: processar a atualização inicial diretamente no iterador do consumidor, antes das mensagens pendentes, sem tentar inseri-la na fila cheia. Bordas iniciam com ForceUpdate para usar o tema/estado atuais. Regressão cobre filas vazias e cheias com capacidades 1, 20 e 50, sem perder mensagens.
- Validação automatizada, build release e MSI atualizado pendentes neste ponto. Não modificar o tema pessoal para contornar o defeito.
- Consulta global-state também encontrou o erro 10022 já registrado. Mantém-se investigação separada: esse erro não é prova de tema inválido.


#### Resultado automatizado da correção de tema

- Suíte serial: **186 passaram, zero falhas e um ignorado histórico**: [log](evidence/theme-startup-tests.log). Regressão de inicialização cobre filas cheias/vazias sem efeitos Win32.
- Release dos seis executáveis passou: [log](evidence/theme-startup-release.log). MSI atualizado gerado em `target/wix/komorebi-0.1.42-native-desktops-theme-fix-x86_64.msi`: [log](evidence/theme-startup-msi.log). Payload extraído e comparado byte a byte aos seis binários: [inspeção](evidence/theme-startup-msi-inspection.log), [conferência](evidence/theme-startup-artifacts.log).
- SHA256 do MSI: `ee4507f2c1fd4a248a1b198e9c757a8fc7a9b5bb15a8d58bf15b54c8a10d36f2`. `git diff --check` passou. Instalador anterior preservado; nenhum MSI foi instalado e o processo instalado não foi reiniciado nesta rodada. Validação visual da borda ativa com o executável corrigido ainda pendente.
- Usuário solicitou orientação para comparar a implementação: branch feat/native-desktop-workspaces, base comum com master 6dbffcc9e9ff26492c435447f0a264bb4068fd21. Diff contra a base inclui commits da migração e mudanças locais rastreadas; novos arquivos exigem leitura separada via git status/editor. A correção das filas também está nas mudanças locais.


### 2026-10-26 — Clareamento transitório do monitor secundário na troca nativa

- Usuário relatou que o secundário fica mais claro antes da troca por Ctrl+Win+setas. Inspeção confirmou transparência=false, animações de movimento habilitadas e hash do daemon instalado igual ao release corrigido. Esses dados, isoladamente, não estabelecem causa.
- Perguntado sobre reprodução sem komorebi, usuário confirmou **“Também acontece com o komorebi fechado”**. Isso enfraquece atribuição ao código da migração: não aplicar correção especulativa de transparência/bordas como solução para esse sintoma.
- Nenhum código/configuração foi modificado para contornar o clareamento. Causa ainda não identificada; próximo comparativo útil é trocar pelo Task View com mouse, mantendo komorebi fechado, para distinguir efeito da transição nativa de algo associado ao atalho. Reprodução sem daemon não identifica por si só driver, HDR ou DWM como causa comprovada.


### 2026-10-26 — Publicação para revisão no perfil do usuário

- Usuário autorizou commit das alterações e PR para main em seu perfil. Fork criado em https://github.com/theviniis/komorebi; remoto local fork adicionado, preservando origin apontando ao projeto original.
- O upstream usa master e não havia main. A main do fork foi criada a partir da master local (6dbffcc9e9ff26492c435447f0a264bb4068fd21), preservando a base da migração. PR será feat/native-desktop-workspaces → main no fork pessoal.
- Escopo do commit: código, regressões, documentação, schemas e evidências do acompanhamento. Arquivos avulsos list.txt/test_output*.txt permanecem locais. Configuração pessoal do whkd não faz parte do repositório.
- PR em rascunho enquanto os critérios manuais restantes, validação visual da correção do tema e erro 10022 continuam abertos. Criação do PR não significa aceite completo nem merge autorizado.

- Preparação do commit: logs de evidência normalizados para UTF-8/LF e espaços finais removidos, sem alterar resultados; conteúdo do patch histórico preservado.
