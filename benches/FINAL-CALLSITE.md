# Fonte final: fechamento por PC/callsite e comparação com Sycamore

## Controle e método

Nenhuma mudança de runtime, proteção, adapter ou atributo de inlining nesta
etapa. Foi usado o MESMO binário profiling selado de `hypothesis-final`, não
um rebuild nem o snapshot outlined. Full/default/produção permanecem intactos.

`final-callsite.json`: 48 processos Callgrind — Demand/Sycamore × Chain/cut ×
set/flush × N=64/256/384 × 200/400 escritas. Fronteiras exatas noinline já
existentes no harness: `reactive[_sycamore]_walk_{set,flush}`; coleção inicia
desligada. `--dump-instr=yes`, Ir apenas, abort efetivo confirmado em cada
processo, sem diagnostics/verification/counters. Nenhum timing novo.

Cada PC exclusivo é somado uma vez; calls inclusivos NÃO são somados de novo.
PCs do executável são resolvidos por `addr2line -C -a -f -i` e ligados ao
assembly (`objdump`). A soma dos PCs deve ser exatamente o summary de cada
trace. Fits por símbolo/callsite separam slope e intercepto; todos os resíduos
são <1e-6 instr/escrita. Em cada callsite e símbolo, 400 escritas devem reproduzir
exatamente 2× o Ir de 200. Contagens de chamadas usam 400−200, pois chamadas
incluem setup mesmo com collect desligado; são contagens do loop inteiro,
não uma decomposição inclusiva por fase.

Atribuição de inline frames é feita DE DENTRO PARA FORA: push/pop agora podem
estar sob next. Escolher next primeiro por nome esconderia esses custos dentro
do laço. Linhas DWARF desconhecidas permanecem explícitas, sem redistribuição.
Isso é localização do trabalho emitido, não orçamento de instruções removíveis.

As fontes externas arquivadas são as REALMENTE indicadas pelo DWARF, em
`sycamore-reactive-0.9.4` do registry, não uma cópia de referência possivelmente
diferente. Hashes e source bundle incluem os arquivos usados de root/node/
signals/memos e helpers externos disponíveis.

## Totais exatos atuais

N=memos. Cada região tem forma `slope*N + intercepto`, instr/escrita:

| Motor/fixture | Set | Flush | Total |
| --- | --- | --- | --- |
| Demand Chain | 58N+423 | **1064N+1385** | **1122N+1808** |
| Demand cut | 58N+423 | **272N+1437** | **330N+1860** |
| Sycamore Chain | **744N+1006** | 0 | **744N+1006** |
| Sycamore cut | **111N+956** | 0 | **111N+956** |

Os slopes Sycamore 744/111 foram REMEDIDOS no binário final, com adapter
corrigido, não importados do checkpoint histórico. Demand coincide exatamente
com todos os slopes/interceptos de `hypothesis-final`.

## Cut Demand: as 272 instruções

| Callsite lógico exclusivo | Instr/memo |
| --- | ---: |
| next, incluindo o laço fundido | **181** |
| push | **61** |
| pop | **28** |
| tick/callsite externo | **2** |
| **Total flush** | **272** |

Todo trabalho da única callback executada por escrita e da Session única
fica no INTERCEPTO, não nesses slopes. Não somar custos por callback aos 272.

As 181 de next fecham por atribuição de linhas do bundle final:

| Família de linhas em fused.rs | Instr/memo |
| --- | ---: |
| Frame/loop/certificado: 150,154,155,158,159,163,165 | **57** |
| Acesso/revisão/gate da fonte: 171,172,175,177,178,186,187,190,199,203,204,207 | **63** |
| State/initialized do memo: 182,183 | **20** |
| Callsite de descida/push: 214,215 | **13** |
| Publicação de Clean: 221 | **1** |
| Método next conhecido, linha DWARF desconhecida | **27** |
| **Total next** | **181** |

Exemplos individuais: linha 183 custa 18; observar revisão na retomada (177)
custa 16; teste do fim do laço (150), 13; last_mut (154), 12; consumir waiting
(165), 12; resolução da fonte (203), 9; revalidação do reader (159), 8.
São instruções ATRIBUÍDAS a essas linhas, incluindo helpers inline, não apenas
o texto da operação aparente. O callsite push (215) tem 12 além do símbolo
push: esses PCs não foram contados duas vezes.

Push 61 inclui 15 atribuídas à publicação do Frame (126), 5 à profundidade
lógica (118), 2 ao limite (120), e entradas/saídas/checks/cálculos restantes.
Pop 28 inclui retirada do Frame (255:11), acesso ao header (259:5), certificado/
geração (258:4), poison (261:3), phase=Idle (260:1) e saída (273:4).

Há 31 instr/memo do cut com linha de origem desconhecida (27 next, 4 push),
mas NÃO com função lógica ou endereço desconhecidos. Seus PCs/asm estão no
JSON. Não foram chamados de spill, prólogo ou overhead removível por palpite.

## Chain−cut Demand: as 792 instruções

Flush diferencial: **792N−52 instr/escrita**. Set cancela exatamente.

| Bloco exclusivo adicional | Instr/memo |
| --- | ---: |
| Completion typed + helper graph de publicação | **250** |
| Read limpo + tracking + notify | **196** |
| Avaliador: Execute, preparação do executable e retomada | **163** |
| reconcile | **107** |
| prepare lógico | **54** |
| Callback/fixture | **18** |
| Corpo de PartialEq | **4** |
| **Total** | **792** |

Fechamentos verificáveis por símbolos físicos:

- Completion: execute::<u64> **203** + helper graph de publicação **47**.
- Read: read_copy **118** + record_resolved_read **63** + notify **15**.
- Avaliador: Runtime::update **154** + Session::pop **9**.

Os 163 também fecham pela representação alternativa de callsites (não somar
ambas): next **28**, execute **51**, resolve_executable **10**, tick **49**,
run **9**, validation **7**, pop **9**; push **0** adicional.

Exemplos de callsites positivos no diferencial: verificação de vida antes de
reconcile (`fused.rs:336`) 21; kernel/callsite tick (275) 19; resolução do reader
no tracking (`lib.rs:1143`) 17; header do source no preflight (1258) 15; preflight
de prepare (`ownership.rs:433`) 14. A lista completa tem TODOS os sinais,
incluindo deltas negativos e linhas desconhecidas; o analisador fecha 792 sem
selecionar só aumentos.

N=256, chamadas diferenciais por escrita: push/pop **257** nos DOIS fixtures;
typed execute **256** na Chain, **1** no cut. Não é uma diferença de número de
frames por caminho, e sim de execução e custo por retorno.

## Sycamore: símbolos comparáveis, não uma caixa-preta de 111/744

Custos exclusivos. Work inlined no adapter set é atribuído à operação Sycamore
que o DWARF identifica, não contabilizado como custo do adapter por suposição.

| Operação/símbolo | cut | Chain | Chain−cut |
| --- | ---: | ---: | ---: |
| Root::dfs | **80** | **80** | 0 |
| Root::propagate_node_updates, scan inlined | **31** | **34** | **3** |
| Root::run_node_update | 0 | **235** | **235** |
| NodeHandle::dispose_children | 0 | **121** | **121** |
| DependencyTracker::create_dependency_link | 0 | **79** | **79** |
| Root::mark_dependents_dirty | 0 | **68** | **68** |
| Read/track | 0 | **92** | **92** |
| Selector callback, comparação/publicação | 0 | **27** | **27** |
| Any::type_id, corpo separado | 0 | **8** | **8** |
| **Total set** | **111** | **744** | **633** |

Read/track 92 = with_untracked **59** + callback-view/get com helpers inline
**33**. Esse 33 NÃO é clone de Runtime: a view Sycamore é sem estado; os frames
inline resolvem signal/get/track. No agrupamento lógico, reparte-se em get 8,
get_untracked 6 e track 19.

Os zeros no cut são slopes zero, NÃO ausência de execução: M1 ainda atualiza,
faz cleanup/tracking/comparação uma vez por escrita; tudo fica no intercepto.
N=256: dfs **258 chamadas/escrita** em Chain e cut. run_node_update,
dispose_children e create_dependency_link executam **257** na Chain e **1**
no cut. Flush é genuinamente vazio neste harness; o trabalho está no set.

O source bundle confirma o mecanismo:

- root.rs:255–277: DFS com marks Temp/Permanent e Vec de ordem reversa.
- root.rs:221–234: scan da ordem, reset de mark, execução só de Dirty.
- root.rs:146–151: run_node_update já remove links antigos; 235 não é só
  dispatch de callback. Não comparar link 79 sozinho com reconcile 107.
- root.rs:160–177: dispose children, tracked_scope/callback, recriar links,
  restaurar callback/value, marcar Clean e notificar dependentes se mudou.
- node.rs:98–120: mesmo sem children/cleanup neste fixture, dispose_children
  faz acesso ao nó, take de buffers, contexto untracked e limpeza de context.

## Ponte exata do gap e o que ela permite concluir

```text
Demand Chain 1122 = cut 330 + adicional 792
Sycamore Chain 744 = cut 111 + adicional 633
Gap Chain     378 = gap cut 219 + gap adicional 159
```

Isso localiza **58% do gap de slope da Chain no orçamento que já existe no
cut**, e 42% no adicional de executar/retomar/publicar. É identidade entre
fixtures, não uma experiência causal de apagar o walk. Interceptos completos:
gap cut `219N+904`, gap adicional `159N−102`, gap Chain `378N+802`.

Separação sustentada pela evidência:

- **Walk/protocolo:** Demand carrega frames, cursor/waiting, fases e certificados
  de geração/revisão; Sycamore faz DFS+ordem e testa Dirty. Ambos percorrem N no
  cut, mas com protocolos diferentes. Os 219 não são só “checks de segurança”.
- **Contratos explícitos:** preflight de primeira leitura, checked_add de
  revisão/epoch/depth, caps/purity, revalidação após callback e recuperação de
  Err/unwind aparecem nos nossos callsites. Isso localiza o trabalho, mas não
  demonstra que sua implementação atual tenha custo mínimo ou irremovível.
- **Trabalho genérico dos dois lados:** Sycamore também paga gerações de SlotMap,
  borrows, ownership/cleanup, tracking e type erasure. Seus 121 de cleanup
  impedem explicar todo o gap como “eles não fazem ownership”. Seus 235 incluem
  unlink; subtrair blocos com fronteiras diferentes produziria uma falsa medida.
- **Blocos agora mensurados para estudo:** completion 250, read/tracking 196,
  evaluator adicional 163 e reconcile 107. Não há autorização para retirar o
  preflight/certificados: a rodada anterior já mostrou um atalho inválido por
  fault injection. Não se propõe aqui outra otimização por leitura de código.

O dump separa ONDE o trabalho está e mostra diferenças de mecanismo. Ainda NÃO
separa numericamente “X eliminável” de “Y preço obrigatório dos contratos”. Isso
exigiria ablações corretas com fronteiras equivalentes, não só símbolos.

## Evidência e reprodução

```sh
python3 benches/profile-final-callsites.py  # só para uma nova coleta sem label existente
python3 benches/analyze-final-callsites.py
```

O primeiro comando recusa sobrescrever a coleta. Dados sob `benches/.log/`:
`final-callsite.json`, `final-callsite-analysis.json`, executável selado, traces,
source bundle, dependency-source-bundle, inline-stacks, disassembly e
`instructions-{topological,sycamore}-{chain,cut}-{set,flush}-256.json`.
Cada registro por PC inclui função física, endereço, Ir, Ir/escrita, asm,
callsite lógico e TODOS os inline frames. Hashes dos artefatos ficam na análise.

Fonte Rust coincide com a validação anterior final (Miri dirigido 13/mode);
nenhuma nova aprovação Miri é alegada. Scripts analisados sintaticamente e
diff-check executado. Sem commit/push, promoção ou novos números de tempo.
