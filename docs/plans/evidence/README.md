# Evidências da validação da migração

Data: 2026-10-25. Worktree de implementação preservada.

## Investigação do estado anterior

Não existe snapshot byte a byte dos arquivos locais anteriores à etapa 1. Por isso a reprodução foi feita em uma exportação temporária do commit `e57c037d995f16d880976c8e4fdf9f79ad06a2fe`, reaplicando o campo `window_desktops`, suas inicializações e os quatro métodos de tracking que já existiam localmente. Isso é uma **reconstrução diagnóstica**, não uma afirmação de recuperação exata do checkout histórico.

O manifesto `stage1-reconstruction.json` registra base, caminho temporário e hashes. `stage1-reconstruction.patch` contém as diferenças sobre a base; seu último teste é uma instrumentação diagnóstica do canal, não código de produção nem teste incluído na suíte atual. Para repetir, exportar a base em diretório separado, aplicar o patch e usar os comandos nos logs. Não aplicar esse patch na worktree de implementação.

| Cenário | Resultado observado | Evidência |
|---|---|---|
| Execução histórica original | 122 passaram, 3 falharam, 1 ignorado; canal vazio, diretório local e movimento | `stage1-historical-suite.log`, cópia do `test_output.txt` preexistente |
| Cada um dos três testes isolado na reconstrução | Os três passaram | `stage1-baseline.log` |
| Suíte serial da reconstrução | 123 passaram, 2 falharam, 1 ignorado; serialização e movimento | `stage1-baseline.log` |
| Serialização isolada com USERPROFILE contaminado | Falhou por ausência de diretório local | `stage1-contextual-reproduction.log` |
| Movimento isolado com USERPROFILE contaminado | Passou; a falha do movimento depende também da sessão/foreground e do contexto da suíte | `stage1-contextual-reproduction.log` |
| Dois consumidores do canal global | Consumidor concorrente retirou a mensagem; o receiver do teste ficou vazio | `stage1-contextual-reproduction.log`, teste diagnóstico determinístico |

Essas diferenças são parte do diagnóstico. Não se afirma que cada falha dependente de contexto deva reaparecer em toda execução isolada. A falha do canal consta no log original; o teste controlado reproduz a condição de disputa sem criar janela oculta nem deixar threads vivas.

`stage1-polluted-environment.log` registra uma tentativa **inválida para diagnóstico**: definir USERPROFILE antes de iniciar Cargo impede o próprio rustup de localizar sua home. Nenhum resultado desse arquivo foi contado como teste executado. A verificação válida compilou com ambiente normal e executou diretamente o binário de teste em processo separado com USERPROFILE alterado; os resultados estão em `stage1-contextual-reproduction.log`.

## Validação do código corrigido

`stages1-2-validation.log` preserva os comandos, resultados intermediários e finais. A primeira repetição revelou dois testes antigos de Monitor ainda dependentes de foreground real (erro Win32 87); a dependência foi substituída pelo fake nesses testes e a validação foi reiniciada. Os resultados anteriores não foram apagados nem apresentados como aprovação final.

A suíte atual inclui testes de restauração após panic para ambiente/backend/caches, listener privado sem consumir o canal global, todas as seis ordens dos eventos Show/Manage/Uncloak repetidas em desktops focados/não focados e janelas tiled/flutuantes, regras entre monitores/desktops, erros antes/depois do movimento, retry de regra inicial, Task View após regra inicial e notificações por socket após mutação parcial.

Os testes de eventos e regras usam identificadores e operações Win32 falsos. O socket de assinante é local ao teste, tem timeout e thread encerrada e juntada no Drop. Os logs de aprovação desta entrega não substituem os cenários manuais e de ciclo de vida das etapas seguintes.

A inclusão de janelas em stacks também foi separada entre bookkeeping e efeitos de visibilidade. O backend real mantém o comportamento anterior; os eventos com backend fake e a preparação das fixtures não chamam HideWindow para HWNDs fictícios.


## Etapa 3 — identidade e ciclo de vida

`stage3-validation.log` registra comandos e resultados dessa etapa. As duas últimas suítes seriais completas passaram com 169 testes aprovados, zero falhas e um ignorado histórico. O check do workspace também passou após as últimas alterações.

O acompanhamento principal descreve as 48 combinações de reordenação/foco, criação/remoção, consulta com falha e retry, nomes, notificações, monitor em cache, proteção do reaper e reordenação durante movimento de stack. Os cenários usam backend e efeitos Win32 falsos. Não houve validação visual ou manipulação de desktops reais para aprovar esta etapa; esses critérios permanecem na etapa 5.

## Integração opcional (2026-10-26)

Arquivos `optional-native-*.log`: testes finais em execução serial, release, schemas e MSI extraído/validado. Ver o [plano de integração opcional](../2026-10-26-optional-native-desktop-workspaces.md) para os critérios ainda dependentes de validação manual.
