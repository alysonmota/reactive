# Continuação de subida: experimento delimitado

## Plano congelado antes da implementação

Controle: fonte final atribuída em [FINAL-CALLSITE.md](FINAL-CALLSITE.md).
Snapshot/hash/HEAD e identidades dos executáveis profiling e timing existentes:
`benches/.log/runs/ascent-control/`. Não reconstruir o controle silenciosamente.

Hipótese: ao terminar a verificação de um filho sem callback, consumir diretamente
a continuação do pai e, se sua última fonte permaneceu igual, encerrá-lo na mesma
subida. Evitar retornar ao despacho geral de `next` para cada ancestral. Fontes
restantes, reader Dirty/não inicializado e mudança de revisão voltam ao despacho
existente. O primeiro retorno de callback continua no caminho checked.

Não é Dirty-direct, certificado entre comparadores, nova arena, nem fusão de
execute/reconcile. Não mudar publicação, leitura/tracking, reconcile, adapters,
ownership, FullFrames ou produção. Manter prepare lógico inline(never).

### Obrigações por transição

| Transição | Validade/ação obrigatória |
| --- | --- |
| Callback → kernel | tick mantém pop checked; epoch invalida certificados suspensos |
| Filho verificado → pai | Validar Key do pai se epoch divergiu; só então acessar cold/hot |
| Pai waiting → observar fonte | Reler Source pelo ordinal; header da fonte resolve geração, inclusive tombstone |
| Revisão mudou | checked freshness, Dirty; despacho existente decide execução |
| Revisão igual | Avançar cursor; só encerrar se initialized, não Dirty e sem outras fontes |
| Encerrar pai | Clean e pop existentes; preservar phase, poison e diagnóstico |
| Reader descartado | Não tocar novo slot; pop checked/certificado conforme protocolo existente |
| Erro/unwind | Propagar; Session::drop recupera todos os frames pelo caminho checked |

Nenhuma referência/cópia de Source atravessa callback. Profundidade, caps/purity,
ciclos e preparação continuam nos caminhos existentes. Nunca acessar frames abaixo
de Session::base. A continuação não pode ignorar erro de geração da fonte, mesmo
que o slot tenha sido tombstonado.

## Gates e critérios de decisão

1. Adicionar testes: subida longa sem execução, múltiplas fontes/última fonte,
   reader Dirty, geração inválida na retomada e recuperação após erro. Comparar
   comportamento em FullFrames/DemandDriven e invariantes finais.
2. fmt + testes workspace fast-dev default/all-features nos dois modos; gates
   Miri dirigidos dos dois modos, incluindo os testes novos, ANTES da coleta.
3. Profiling effective-abort, set/flush separados, N=64/256/384. Aprovação:
   cut flush <=240 (controle 272), Chain <=1064, Diamond <=1517,
   Fan-out <=2252.75 instr por denominador do fixture; verificar também set.
   240 é limiar de utilidade, não previsão. Full deve continuar controle sem
   mudança semântica; qualquer codegen diferente será reportado.
4. Timing release pareado contra `hypothesis-final-time`, CPU 2, fixtures
   Chain/cut/Diamond/Fan-out/Conditional. Exigir ganho no cut e nenhuma regressão
   consistente; resultado ruidoso/inconclusivo não aprova a hipótese.
5. Dump por PC da variante para explicar remoção/deslocamento, sem estimar pela
   fonte. Registrar inclusive uma variante rejeitada, com snapshot e hashes.

Um protótipo e no máximo uma revisão corretiva motivada por evidência concreta.
Falha de gate bloqueia performance. Não empilhar outras otimizações. Se rejeitado,
restaurar somente as mudanças experimentais de runtime feitas nesta rodada,
preservando testes pertinentes e evidência. Sem commit/push/promoção solicitados.

## Resultado

### Protótipo ascent-v1 — reprovado nos critérios

Fonte preservada em `benches/.log/runs/ascent-v1/source-bundle.json`, com
executáveis selados e validação source-matched. Nativo all-features: 495 testes
por modo; Miri dirigido: 16 por modo (incluindo os três testes novos). Default
workspace também passou nos dois modos; Clippy all-targets/all-features passou.

| Flush slope | Controle | v1 | Delta |
| --- | ---: | ---: | ---: |
| Demand Chain | 1064 | 1089 | +25 |
| Demand cut | 272 | 257 | -15 |
| Demand Diamond | 1517 | 1541 | +24 |
| Demand Fan-out | 2252.75 | 2301.75 | +49 |

Set e slopes Full permaneceram iguais. Cut não chegou a <=240 e os outros
três fixtures regrediram em instruções. Timing pareado variant/control Demand:
Chain 1.0088, cut 1.0055, Diamond 1.0320, Fan-out 1.0061, Conditional 1.0034.
Cut teve ambos os blocos acima de 1, não evidenciando ganho em tempo. São
resultados descritivos de dois blocos ABBA, não teste de significância.

`ascent-v1-analysis.json` verifica hashes, gates, critérios e o dump por PC de
`ascent-v1-pcs`. No cut, next caiu 181→168 e tick 2→0; push/pop ficaram 61/28.
Na Chain, o aumento fecha: next +11, resolve_executable +12, execute +1, tick +1.
O assembly mostra resolve_executable inline no controle (10 instr/memo), mas
outlined na v1 (22), com chamada/protocolo e cálculos/checks adicionais.
Não inferimos spills/cache/IPC a partir disso.

### Única revisão corretiva: ascent-v2

Mudança isolada: mover a mesma continuação para `Session::ascend`, inline(never).
Sem mudar suas condições, checks ou fallback. A correção testa isolamento de
codegen do dispatcher geral, motivado pela perda de inlining MEDIDA na v1.
Na cadeia igual, uma chamada contém toda a subida (não uma chamada por memo).
Novos gates completos antes de performance; mesmos critérios, sem terceira
variante ou mudança do alvo. Nenhuma das variantes está aprovada até decisão.

### Decisão: rejeitar a transformação, sem terceira variante

V2 passou novamente nos 495 testes nativos all-features e 16 testes Miri
dirigidos por modo, default nos dois modos e Clippy. Custos:

| Flush slope | Controle | v1 | v2 | Delta v2/controle |
| --- | ---: | ---: | ---: | ---: |
| Demand Chain | 1064 | 1089 | 1088 | +24 |
| Demand cut | 272 | 257 | 243 | -29 |
| Demand Diamond | 1517 | 1541 | 1549 | +32 |
| Demand Fan-out | 2252.75 | 2301.75 | 2309.75 | +57 |

Set e slopes Full continuaram iguais. V2 não atinge o limiar do cut por 3
instruções, mas a rejeição NÃO depende dessa pequena diferença: os outros
três fixtures falham claramente os critérios de não regressão.

Timing pareado variant/control Demand, v2:

| Fixture | Razão agregada | Bloco 1 | Bloco 2 |
| --- | ---: | ---: | ---: |
| Chain | 1.0542 | 1.0357 | 1.0730 |
| cut | 0.9883 | 1.0025 | 0.9742 |
| Diamond | 1.0438 | 1.0261 | 1.0617 |
| Fan-out | 1.0699 | 1.0708 | 1.0689 |
| Conditional | 1.0211 | 1.0238 | 1.0183 |

O ganho agregado do cut é inconsistente entre os blocos; não sustenta aprovação.
Full Chain/Diamond também variaram em tempo apesar de slopes iguais; não
atribuímos causalidade microarquitetural nem significância a esses dois blocos.
Não há nova coleta de referência Criterion nem comparação nova com Sycamore.

Runtime experimental removido após finalizar o dump por PC v2. Testes
dirigidos e os dois snapshots/executáveis permanecem, bem como os coletores e
`analyze-bounded-ascent.py`. A mudança proposta não se mostrou uma melhoria global;
isso não demonstra que todo redesenho de caminhada seja inútil.

O dump v2 fecha cut **243 = next 81 + ascend 73 + push 61 + pop 28**.
Comparado ao controle: next -100, novo ascend +73, tick -2 = **-29**.
Push/pop não diminuíram: houve redução/redistribuição do despacho, não remoção
do protocolo de frames. Na Chain, os **+24** fecham em next +4,
resolve_executable +12, execute +4, run +1 e tick +3. Outlining da subida NÃO
recuperou o inlining de resolve_executable; a hipótese corretiva falhou.
Nenhum delta foi atribuído a spills/cache/latência por estimativa.

Após remoção, `fused.rs`, `ownership.rs`, `evaluation.rs` e produção são
verificados contra o snapshot original. Só testes e documentação/ferramentas
da rodada permanecem; nenhum ganho de runtime é retido nem alegado.

Reprodução da análise (não sobrescreve traces/binários):

```sh
python3 benches/analyze-bounded-ascent.py ascent-v1 --callsites ascent-v1-pcs
python3 benches/analyze-bounded-ascent.py ascent-v2 --callsites ascent-v2-pcs
```

Após restauração, `current_source_matches_variant` deve ser false: o analisador
continua validando os bundles/identidades arquivados, sem fingir que a fonte
ativa é uma das variantes. Não aplicar os tempos medidos às fontes restauradas.

Restauração revalidada: default e all-features nos dois modos, 16 testes Miri
dirigidos por modo, Clippy all-targets/all-features, fmt e diff-check passaram.
`ascent-restored-validation.json` e `ascent-restoration.json` preservam os gates
e hashes. O analisador histórico `analyze-ascent.py` permanece byte-idêntico;
esta rodada usa um nome separado para não substituir evidência anterior.

Os analisadores históricos que exigem igualdade de TODAS as fontes do checkpoint
final podem agora recusar os três testes novos. Isso não é uma mudança do runtime
nem motivo para enfraquecer suas verificações; usar o analisador desta rodada
para os bundles arquivados e a validação source-matched da restauração.
