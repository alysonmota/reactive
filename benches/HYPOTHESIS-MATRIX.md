# Hipóteses isoladas após kernel-final-directed

> Atribuição posterior da fonte final, sem mudar runtime:
> [FINAL-CALLSITE.md](FINAL-CALLSITE.md).

## Método e estado

Controle inicial: `kernel-final-directed`, já contendo publicação fundida e
reconcile de uma passada. As hipóteses individuais usam esse controle, exceto
lazy-capture, que usa o adapter corrigido (explicitado abaixo). A comparação
combined-dirty mede o efeito marginal contra combined, não contra o inicial.
Variantes ficam seladas sob `benches/.log/runs/`; a decisão de combinação é
separada das medições individuais. FullFrames continua default, sem produção,
commit ou push.

Cada variante correta passou primeiro workspace all-features e Miri dirigido
nos dois modos, depois 48 processos Callgrind (set/flush, N=64/256/384, 200
escritas, sem diagnostics/verification, abort efetivo). Fits com resíduo <1e-6
instr/escrita; as fontes das validações e binários são verificadas pelo analisador.

Tempo é OUTRO executável, release `bench-abort`, `-C panic=abort` verificado
por processo, CPU 2, fixed-work N=256, 5000 escritas, ABBAABBA por modo/fixture,
4 amostras por braço. Inclui set+flush e laço do fixture. Não misturar esses
tempos com Criterion ou profiling. Sem isolamento físico/teste de significância;
diferenças pequenas e inconsistentes não estabelecem ganho em latência.

Ferramentas:

```sh
python3 benches/time-hypothesis.py hypothesis-control
python3 benches/run-hypothesis.py NOVO_LABEL
python3 benches/analyze-hypotheses.py LABEL_1 LABEL_2
```

O primeiro comando sela o controle release antes das edições. O runner faz fmt,
native/Miri, Callgrind e timing, em sequência fail-closed. Uma falha bloqueia
TODAS as medições daquela fonte. Snapshots incluem fontes, binários, hashes,
comandos e logs de testes. Nenhum resultado é subtraído por estimativa.

Inventário fechado: **17 hipóteses implementadas**, 16 passaram os gates e foram medidas; uma foi
reprovada por contrato antes de performance. Há ainda duas composições medidas
e uma coleta final após remover métodos mortos do adapter. Erros de compilação
intermediários do teste/adapter estão nos logs, não foram contados como rejeição
de hipótese. O hoist ingênuo também foi corrigido antes de receber métricas.

## Resultados individuais: instruções

Slopes de flush Demand; ordem Chain/cut/Diamond/Fan-out. N significa memos,
memos, branches e pares memo/efeito, respectivamente.

| Hipótese / sufixo do label `hypothesis-` | Chain | cut | Diamond | Fan-out |
| --- | ---: | ---: | ---: | ---: |
| **Controle** | **1179** | **330** | **1635** | **2493,75** |
| alive-preflight | 1174 | 330 | 1632 | 2478,75 |
| execute-header | 1179 | 330 | 1635 | 2493,75 |
| prepared-descriptor | 1117 | 308 | 1578 | 2374,75 |
| read-hoist-live | 1165 | 317 | 1620 | 2464,75 |
| action-error | 1171 | 321 | 1638 | 2489,75 |
| source-fields | 1179 | 330 | 1635 | 2493,75 |
| split-borrows | 1168 | 320 | 1636 | 2483,75 |
| descent-certificate | 1176 | 326 | 1641 | 2497,75 |
| depth-deferred | 1177 | 318 | 1629 | 2485,75 |
| caps-context | 1167 | 318 | 1623 | 2469,75 |
| validation-context | 1169 | 330 | 1623 | 2471,75 |
| graph-frames | 1182 | 339 | 1645 | 2506,75 |
| tight-walk | 1140 | 310 | 1598 | 2417,75 |
| direct-dirty-ready | 1205 | 337 | 1418 | 2561,75 |
| adapter-borrowed-ready | 1170 | 330 | 1626 | 2474,75 |
| lazy-capture¹ | 1167 | 341 | 1632 | 2477,75 |
| combined | 1064 | 272 | 1517 | 2252,75 |
| combined-dirty² | 1089 | 272 | 1403 | 2329,75 |
| **final** | **1064** | **272** | **1517** | **2252,75** |

¹ Controle adapter=1170/330/1626/2474,75; não subtrair do controle inicial para
atribuir ganho à captura. ² Controle combined=1064/272/1517/2252,75.

Full, mesmos slopes/unidades:

| Hipótese | Chain | cut | Diamond | Fan-out |
| --- | ---: | ---: | ---: | ---: |
| **Controle** | **1210** | **439** | **1537** | **2424,75** |
| alive-preflight | 1205 | 439 | 1534 | 2409,75 |
| execute-header | 1210 | 439 | 1537 | 2424,75 |
| prepared-descriptor | 1210 | 439 | 1538 | 2425,75 |
| read-hoist-live | 1219 | 445 | 1544 | 2440,75 |
| action-error | 1210 | 439 | 1538 | 2425,75 |
| source-fields | 1210 | 439 | 1538 | 2425,75 |
| split-borrows | 1210 | 439 | 1538 | 2425,75 |
| descent-certificate | 1210 | 439 | 1538 | 2425,75 |
| depth-deferred | 1210 | 439 | 1538 | 2425,75 |
| caps-context | 1210 | 439 | 1537 | 2424,75 |
| validation-context | 1210 | 439 | 1538 | 2425,75 |
| graph-frames | 1210 | 439 | 1538 | 2425,75 |
| tight-walk | 1201 | 430 | 1539 | 2417,75 |
| direct-dirty-ready | 1219 | 445 | 1544 | 2440,75 |
| adapter-borrowed-ready | 1201 | 439 | 1528 | 2405,75 |
| lazy-capture | 1201 | 439 | 1529 | 2406,75 |
| combined | 1196 | 439 | 1526 | 2391,75 |
| combined-dirty | 1196 | 439 | 1525 | 2390,75 |
| **final** | **1196** | **439** | **1526** | **2391,75** |

O Full também foi medido em TODAS as variantes; resultados integrais em
`hypothesis-analysis.json`. Apesar de mudanças apenas no fused, houve mudanças
de codegen no controle Full: por exemplo read-hoist-live Full=1219/445/1544/
2440,75 contra 1210/439/1537/2424,75, e tight-walk Full=1201/430/1539/2417,75.
Não presumir que editar só o evaluator Demand preserva o código gerado do Full.

## Resultados individuais: tempo

Razão pareada variante/controle, Demand, menor é melhor. Cada linha tem seu
próprio controle medido na mesma sequência, não um tempo histórico comum.

| Hipótese | Chain | cut | Diamond | Fan-out | Conditional |
| --- | ---: | ---: | ---: | ---: | ---: |
| alive-preflight | 0,9574 | 1,0125 | 0,9639 | 0,9710 | 1,0150 |
| execute-header | 0,9847 | 0,9691 | 0,9849 | 1,0875 | 0,9639 |
| prepared-descriptor | 0,9780 | 0,9678 | 0,9894 | 0,9930 | 0,9611 |
| read-hoist-live | 1,0010 | 0,9711 | 1,0015 | 1,0193 | 0,9980 |
| action-error | 1,0110 | 0,9582 | 0,9895 | 1,0102 | 0,9619 |
| source-fields | 1,0289 | 0,9345 | 1,0277 | 1,0196 | 1,0121 |
| split-borrows | 1,0074 | 0,9854 | 1,0292 | 1,0235 | 0,9902 |
| descent-certificate | 0,9790 | 0,9574 | 1,0002 | 0,9882 | 0,9902 |
| depth-deferred | 0,9787 | 0,9221 | 0,9944 | 0,9873 | 0,9935 |
| caps-context | 0,9786 | 0,9906 | 0,9903 | 0,9864 | 1,0015 |
| validation-context | 0,9672 | 1,0217 | 0,9931 | 0,9956 | 1,0002 |
| graph-frames | 1,0028 | 0,9291 | 1,0216 | 0,9943 | 1,0152 |
| tight-walk | 0,9839 | 1,0091 | 0,9914 | 0,9669 | 0,9826 |
| direct-dirty-ready | 1,0201 | 0,9711 | 0,8711 | 1,0419 | 0,9052 |
| adapter-borrowed-ready | 1,0136 | 0,9479 | 0,9917 | 0,9861 | 0,9768 |
| lazy-capture¹ | 0,9857 | 0,9616 | 0,9917 | 1,0094 | 0,9906 |
| combined | 0,9396 | 0,8244 | 0,8866 | 0,9040 | 0,9360 |
| combined-dirty² | 1,0453 | 1,0008 | 0,9355 | 1,0372 | 0,9477 |
| **final** | **0,9325** | **0,8578** | **0,8581** | **0,8800** | **0,9236** |

¹ Contra adapter corrigido. ² Contra combined. Todas as demais contra o controle
inicial, incluindo final. As médias absolutas em ns e ambos os ciclos de razões,
por modo/fixture/variante, estão nos manifests `LABEL-time.json` e no analisador.

Exemplo de cautela: source-fields e execute-header não mudam os slopes Demand,
mas suas razões de tempo variam. Esses números NÃO demonstram que copiar Source
era um custo de 40 bytes nem que remover header era um ganho de memória.

## O que cada variante realmente muda

- **alive-preflight:** salva resultado do preflight no mesmo kernel que resolve
  vida. Só valida reads quando compute teve sucesso. Restaura compute e protege
  candidato ANTES de propagar o resultado salvo; erro/panic do compute e check
  de status mantêm precedência. Não atravessa comparador com certificado novo.
- **execute-header:** apenas substitui header_mut após header bem-sucedido pelo
  acesso hot já resolvido. Nenhuma mudança na fronteira pós-callback/reconcile.
- **prepared-descriptor:** Action transporta executable só após prepare lógico
  no mesmo kernel. Fase Executing e diagnostics são registrados ali. Caminho
  físico resolve novamente APÓS cleanup, sem ponteiro atravessando callback.
  O ganho linear no cut, apesar de uma callback/escrita, mostra efeito no walk
  gerado; não pode ser atribuído apenas à remoção de E por callback.
- **read-hoist-live:** check READ uma vez por tick que ainda tem frames, após
  restoration/pop do callback anterior. A versão ingênua read-hoist falhou
  `shutdown_inside_effect_detaches_every_root_before_callbacks` (RuntimeClosed
  em vez de sucesso): verificava status mesmo quando já não havia trabalho.
  Essa versão INCORRETA não foi perfilada nem cronometrada.
- **action-error:** tick retorna Result<Action, ()>; erro fica na Session até
  run propagá-lo. next mantém o Result interno inline. Isso testa essa ABI de
  saída, não prova que todo custo atribuído a tick era spill do Error.
- **source-fields:** extrai só Key/revision/live no avanço e Key/revision na
  retomada; remove o helper que recebia Source inteiro. Sem ganho de slope.
- **split-borrows:** mantém sources emprestado de cold durante a visita e faz
  changed receber apenas hot. Não retém slices/refs entre callbacks, nem elimina
  resolução de geração das fontes. Mantém Source copiado para isolar a hipótese.
- **descent-certificate:** next valida phase e registra Cycle antes da descida;
  push known não repete phase/Clean. Não elimina detecção de ciclo. O próximo
  frame só recebe o certificado dentro do mesmo kernel callback-free.
- **depth-deferred:** cada push continua checked_add e limite 512; publica depth
  antes de prepare/Execute e restaura na saída, sem writes por pop. Diagnostics
  mantém active por push/pop. Miri inclui o teste dirigido de profundidade.
- **caps-context:** efeitos só podem ser raiz, nunca fonte. Assim caps/pure do
  frame derivam de Context e Kind, em vez de reler o frame pai a cada push.
- **validation-context:** só adia owner/pending_owner para restore final; mantém
  captura completa, reader/caps/pure/runtime e o caminho físico de cleanup.
- **graph-frames:** elimina RefCell, move arena para Graph e usa lease segura
  que devolve o Vec mesmo no unwind. Não cria referências &mut sobrepostas a
  Graph e seu Vec. Diagnostics e testes observam a arena real migrada, não uma
  arena antiga vazia. Esta implementação regrediu todos os slopes Demand.
- **tight-walk:** funde descida/pop no próprio laço next, sem Action Descend/Done
  intermediárias. Ainda verifica gerações, cursor e limite a cada push. Não
  alega um mínimo ótimo de armazenamento nem elimina certificados pós-callback.

## Hipóteses restantes: executadas

- **direct-dirty-ready:** initialized Dirty memo demandado dentro de callback
  executa sem Frame. A Session ainda captura/restaura Context; um guard separado
  restaura phase/diagnostics/depth em sucesso/erro/unwind, com geração revalidada
  após callback/cleanup. Initial/Check continuam iterativos para preservar lazy
  demand, ciclos e o limite 512. Novo teste dirigido cobre sucesso/Err/unwind e
  falha de cleanup físico nesse caminho. A implementação isolada melhora muito
  Diamond/Conditional, mas piora Chain/Fan-out.
- **adapter-borrowed-ready:** Engine recebe uma view emprestada por GAT/HRTB.
  Local e Typed não fazem clone/drop de Runtime por callback; Sycamore/Leptos
  recebem views sem estado. Sem unsafe, ponteiro escapado ou captura de Runtime
  forte que criaria ciclo. Smoke valida os adapters/fixtures. Ganho Chain foi
  **9 instr/memo**, não subtração estimada de 27.
- **lazy-capture:** só captura owner/pending na primeira preparação que os pode
  alterar, e só restaura se capturados. Precisa também não restaurá-los em
  validation; essa dependência foi agrupada e medida contra o adapter corrigido.
  Todos os outros campos de Context realmente mudam no protocolo. Passou, mas
  cut **330→341** e Diamond **1626→1632** não justificaram mantê-la nesta composição.
- **preflight-certificate:** implementado e rejeitado ANTES de performance.
  Um novo teste privado altera freshness da fonte no comparador, após preflight.
  O controle deixa o memo Check; a variante deixa Clean incorretamente, nos dois
  modos. Caps puras não autorizam ignorar esse contrato de fault injection. A
  fonte rejeitada e logs estão arquivados; não há timing/Callgrind dela. Isso é
  falha semântica demonstrada, não alegação de UB. Preflight não foi removido.

## Composição final e decisão

Mantidos: alive-preflight, executable preparado, hoist READ corrigido, depth
adiado, caps derivadas de Context, validation sem owner/pending intermediários,
laço fundido e adapter emprestado. Publicação/reconcile anteriores e o prepare
outlined continuam. Ganhos foram medidos isolados ANTES da composição, sem
alegar aditividade; o resultado combinado foi medido de novo.

Não empilhados: Action com erro externo, split-borrows e certificado de descida
têm regressões em outros fixtures; header/Source escalar não reduziram slopes;
arena em Graph e captura lazy regrediram. Os snapshots preservam essas variantes
como alternativas medidas, sem mudar controles/defaults para esconder regressões.

O Dirty direto foi ainda medido SOBRE combined: Diamond **1517→1403**, tempo
**0,9355×**, Conditional **0,9477×**; porém Chain **1064→1089**, tempo **1,0453×**,
e Fan-out **2252,75→2329,75**, tempo **1,0372×**. Não foi empilhado na fonte final
por esse tradeoff concreto, não por complexidade ou ganho pequeno. A variante
alternativa está selada como combined-dirty, com instruções e tempos próprios.

Final só remove métodos mortos do adapter em relação a combined, com os mesmos
slopes/interceptos; recebeu nova validação, profiling e timing. Na fonte final,
Miri dirigido passou **13 testes por modo**, incluindo overflow, disposal/reuso,
provisional reads, cleanup, stale-first-read, profundidade e os novos casos
Dirty/comparador. Não é Miri de todos os ports nem revisão unsafe independente.
Workspace default nos dois modos, Clippy all-targets/all-features `-D warnings`,
fmt, diff-check e sintaxe Python passaram; logs/hashes em
`hypothesis-final-checks.json`. O analisador confirma que produção, ownership
outlined e código do FullFrames permanecem byte a byte iguais ao controle.

## Tempos atuais contra referências: coleta independente

`current-hypothesis-references.json`, fonte Rust final exata: um executável
bench-abort, CPU 2, 20 blocos simétricos, quatro amostras por modo, cinco fixtures
N=256. Criterion, µs/op, não misturar com os fixed-work das ablações:

| Fixture | Produção | Full | Demand | Sycamore | Leptos |
| --- | ---: | ---: | ---: | ---: | ---: |
| Chain | 63,506 | 38,385 | **34,539** | 22,338 | 56,705 |
| cut | 19,329 | 16,529 | **10,521** | 5,358 | 23,400 |
| Diamond | 63,927 | **47,479** | 48,612 | 68,766 | 199,495 |
| Fan-out | 96,096 | 78,632 | **73,603** | 84,589 | 224,389 |
| Conditional | 73,082 | 67,380 | **65,267** | 100,103 | 337,605 |

São tempos atuais, não previsão por instruções. Comparação causal das mudanças
usa o braço controle das ablações, não diferenças entre esta coleta e tabelas
antigas. Ganhos contra referências não estabelecem equivalência de contratos.
