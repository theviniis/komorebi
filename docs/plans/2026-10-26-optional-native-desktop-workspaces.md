# Integração nativa opcional

Opção `native_desktop_workspaces`: ausente/false usa workspaces tradicionais; true usa áreas nativas. Mudar exige reiniciar. Não alterar configuração pessoal.

## Implementação
- [x] Registrar plano e critérios de aceite.
- [x] Configuração, geração e schemas com padrão false.
- [x] Resolver modo antes da inicialização; gate de versão e listener apenas no nativo.
- [x] Restaurar workspaces tradicionais independentes, comandos, nomes e visibilidade.
- [x] Selecionar modo em regras, descoberta, eventos, estado e hotplug; preservar correções compartilhadas.
- [x] Publicar modo e adaptar barra por monitor/global.
- [x] Recarga avisa sobre mudança, preserva modo; ApplyState não troca modo.
- [x] Documentação, testes, release e instalador verificado.

## Critérios de aceite (todos obrigatórios)
- [x] Ausente e false equivalentes.
- [x] Tradicional não consulta backend nativo nem inicia listener ou exige gate nativo; backend que falha em qualquer chamada nos testes.
- [x] Monitores tradicionais com contagens e focos independentes; barra e comandos corretos.
- [ ] Criar/remover/nomear/navegar/mover/regras/visibilidade/restauração tradicionais funcionam — cobertura unitária aprovada; confirmação com janelas reais ainda pendente.
- [x] Regressões nativas passam, incluindo limites e hotplug.
- [x] Recarga e ApplyState preservam modo; reinício aplica opção.
- [x] Configuração gerada e schemas corretos.
- [x] Suítes e release passam; MSI contém executáveis atualizados.
- [ ] Validação manual de ambos os modos sem perda, duplicação ou ocultação indevida (aguarda usuário).

## Referência e evidências
Implementar na worktree da migração, preservando commits posteriores ao PR inicial e arquivos pessoais não rastreados. Referência tradicional: commit 6dbffcc9. Atualizar este documento conforme evidências reais; não marcar validação manual por testes automatizados.

### Implementação concluída; aceite manual pendente

- Modo resolvido no preload; ReplaceConfiguration preserva o modo da instância.
- Comportamento tradicional restaurado em módulos próprios, com referência ao commit anterior à migração. Correções compartilhadas de notificações e reaper mantidas.
- Testes adicionados: opção ausente/false/true, backend proibido no tradicional, foco/contagem/nome por monitor, transferência com índices diferentes, estado legado e barra nos dois modos.
- Execução serial inicial: 195 testes do daemon e 6 da barra aprovados, 1 teste histórico ignorado. Rodada final após revisão da publicação de eventos, recarga real e atualização do cache tradicional: mesmos resultados aprovados.
- Testes existentes compartilham backend/ambiente globais e devem ser executados com `--test-threads=1`; execução paralela expôs interferência entre fixtures, sem evidência de falha de produção.

## Evidências finais automatizadas

- `cargo test -p komorebi -p komorebi-bar -p komorebi-client -p komorebic --all-targets -- --test-threads=1`: **201 aprovados, 0 falhas, 1 ignorado histórico** (rate limiting externo). [Log](evidence/optional-native-tests.log).
- Build release dos seis pacotes concluído. [Log](evidence/optional-native-release.log). Mantidos avisos preexistentes sobre parâmetro não utilizado de bordas e dependência net2.
- Cinco schemas gerados pelos executáveis release: JSON e referências locais válidas; flag opcional com default false. `komorebi --help` e `komorebic --help` encerram com código 0; comando tradicional de swap novamente visível. [Validação](evidence/optional-native-schemas.log).
- MSI: `target/wix/komorebi-0.1.42-optional-native-desktops-x86_64.msi`, versão 0.1.42, x64, 28.336.128 bytes; SHA-256 `58a0be765986997df1f5ddd625862bce63b3be173cc8ea8edbac5c3e0905a2bf`.
- Extração confirmou os seis executáveis byte a byte contra release e a licença contra a fonte. [Hashes](evidence/optional-native-artifacts.log), [build](evidence/optional-native-msi.log), [inspeção](evidence/optional-native-msi-inspection.log). O decompilador emitiu os avisos DARK1059 preexistentes de tabelas UI; payload extraído e validado. O instalador não foi executado.
- `git diff --check` aprovado. Arquivos pessoais não rastreados preservados; configuração pessoal não alterada.

## Interfaces e compatibilidade

- `StaticConfig.native_desktop_workspaces` e `State.native_desktop_workspaces`: booleano, default false; configuração gerada reflete o modo ativo.
- O modo é fixado no preload. Recarga e substituição de configuração avisam sobre mudanças e preservam o modo; outras configurações continuam sendo aplicadas. Teste de recarga real confirmou aplicação de resize_delta=77 com modo preservado.
- Gate Windows 11 24H2 e listener winvd somente no nativo. A admissão de janelas recebe explicitamente o modo para impedir consultas nativas no caminho tradicional, inclusive em janelas cloaked.
- Eventos de entrada/saída do desktop associado e suporte da barra restaurados para o tradicional. Modo nativo mantém foco global; tradicional lê foco/contagem de cada monitor.
- ApplyState não altera o modo. Estado nativo é rejeitado no tradicional com erro explícito; no nativo, estados legados continuam sendo reconciliados com a associação real das janelas.
- Módulos `traditional` preservam os caminhos anteriores de gerenciamento, eventos e hotplug; publicação de eventos, inicialização das filas e atualização dos caches permanecem compartilhadas onde aplicável.

## Validação manual restante (obrigatória para o aceite final)

Testar com o novo MSI ou com os executáveis de `target/release`. Registrar versão do Windows, quantidade de monitores e resultado de cada cenário. Alterar a opção exige reiniciar **komorebi**, não o PC.

- [ ] Com opção ausente/false, iniciar com dois monitores e números diferentes de workspaces. Confirmar nomes, foco e barra independentes; focar workspace em um monitor não muda o outro.
- [ ] No tradicional, criar workspace, garantir quantidade/nomes, navegar com wrap e fechar workspace vazio sem nome. Confirmar ocultação dos inativos e restauração ao retornar.
- [ ] No tradicional, mover janelas tiled, stacks e floats com e sem acompanhar; mover/swap workspaces entre monitores; confirmar regras de posicionamento e ausência de duplicação/perda.
- [ ] No tradicional, reiniciar/restaurar estado e desconectar/reconectar monitor; todas as janelas devem continuar acessíveis.
- [ ] Com true após reiniciar, confirmar Task View, nomes nativos, foco global da barra, limites das setas, movimentação e hotplug sem regressões. Criar/remover desktops pelo Windows.
- [ ] Em cada modo, mudar a opção para o oposto e recarregar: deve haver aviso de reinício e o modo ativo permanecer igual; outro ajuste deve aplicar. Reiniciar e confirmar o novo modo.
- [ ] Alternar entre os modos e executar dez trocas em cada um, verificando janelas sem perda, duplicação ou ocultação indevida e bordas/tema corretos.

Os critérios manuais permanecem abertos. Não declarar a migração totalmente aceita enquanto estes resultados não forem confirmados.
