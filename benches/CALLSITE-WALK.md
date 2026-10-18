# Walk atual: dump por instrução, diferencial e custo de Session

> Este documento atesta o checkpoint outlined. Mudanças posteriores e fonte
> atual: [KERNEL-ABLATIONS.md](KERNEL-ABLATIONS.md). Os custos de execução e
> Chain−cut abaixo não devem ser apresentados como custos da fonte posterior.

## Estado e método

Não houve otimização de runtime nesta etapa. A fonte Rust final é byte a byte
igual à variante `logical-prepare-outlined` reaplicada. FullFrames permanece
default. As únicas mudanças temporárias eram atributos noinline para o probe;
foram removidas, sem descartar qualquer outro trabalho.

O timing atual JÁ existe: `current-outlined-references.json`, reproduzido em
[LOGICAL-PREPARE.md](LOGICAL-PREPARE.md). A afirmação anterior de ausência de
timing não se aplica à fonte reaplicada. Esta etapa não coletou novos tempos.

72 dumps com `--dump-instr=yes`, `--collect-atstart=no` e
`--toggle-collect=reactive_walk_flush`:

- normal: 2 modos × 4 fixtures × N=64/256/384 × 200/400 escritas = 48;
- probe de entrada: 2 modos × Diamond/Fan-out × mesmas escalas/iterações = 24.

O normal usa o MESMO executável profiling selado da medição outlined, sem
rebuild, diagnostics ou verification, com abort efetivo verificado por processo.
Os totals coincidem com todos os slopes/interceptos anteriores. O probe tem
outro executável selado e não substitui o normal como baseline.

Cada endereço é resolvido por `addr2line -C -a -f -i` no executável correto.
Cada instrução exclusiva é atribuída UMA vez ao callsite lógico recuperado;
callees não são somados novamente aos inclusivos dos callers. Somar todos os
PCs deve reproduzir exatamente o summary de cada trace. Os custos por callsite
são ajustados por escala, separando intercepto; todos os resíduos são <1e-6
instr/escrita. 400 escritas reproduzem exatamente 2× o Ir coletado com 200 em
cada callsite. Contagens de chamadas usam a diferença 400−200, pois incluem
warmup mesmo com collection desligada.

DWARF otimizado pode atribuir instruções de next à linha de sua chamada em
tick. Por isso os rótulos abaixo são ATRIBUIÇÃO, não decomposição causal de
overhead eliminável, nem limite inferior de cada operação.

## Cut: onde estão as 330 instruções

Slopes Demand, flush, por memo:

| Callsite lógico | Instr/nó |
| --- | ---: |
| next/step | **128** |
| push | **77** |
| pop | **40** |
| tick/callsite do avanço | **85** |
| **Total** | **330** |

Push inclui 4 e pop inclui 1 instrução/nó sem callsite de projeto recuperável
(prólogos/epílogos). Não foram redistribuídas por palpite. Todo trabalho de
callback, Session única e scheduler do cut fica no intercepto de 1565 instr/
escrita, não nesses slopes: flush = **330N+1565**.

Principais linhas do bundle normal `fused.rs`:

| Callsite | Instr/nó | Operação atribuída |
| --- | ---: | --- |
| 275 | **47** | `self.next(...)`, incluindo trabalho inlined sem frame next recuperado |
| 154 | **16** | `rt.check(READ)` nas duas visitas |
| 130 | **14** | publicação/cópia do Frame no Vec |
| 241 | **14** | retirar Frame do Vec |
| 177 | **13** | observar revision da fonte após retorno |
| 155 | **12** | obter `frames.last_mut()` |
| 278 | **12** | callsite de push no tick |
| 124 | **10** | herdar caps/pure do frame pai |
| 190 | **10** | acesso ao source pelo cursor |
| 171 / 160 / 203 | **9 cada** | reler source esperado / revalidar reader / resolver source |
| 272 / 276 | **9 cada** | verificar fim da pilha / selecionar Action |

Na linha 275 o assembly contém, entre outras instruções, cálculo de índices
Hot/Cold (`lea`, `shl`), loads/comparações e empacotamento de Action. next está
inlined: **não são 47 instruções de chamada de função**. Também não são todas
removíveis descartando o match de tick. O JSON por PC inclui endereço, asm,
função física, Ir e callsite para inspecionar essa atribuição.

Os candidatos menores ficaram quantificados: depth.set no push = **4**
instr/nó (linha 145), no pop = **3** (258); leitura de caps/pure do pai = **10**
(124). Há também 2 atribuídas ao cálculo/endereço anterior ao set do pop
(255), que não constituem ganho garantido. Portanto nem depth nem herança
de caps, isoladamente, explicam a distância até 150–200.

Há 2N+1 visitas a next e N+1 push/pop tanto na Chain quanto no cut, como já
verificado no probe diferencial anterior. O 128 por nó de next abrange as
duas visitas, aproximadamente 64 por visita no caminho estável. Isso NÃO
estabelece que o protocolo atual seja necessário: localiza o restante no
protocolo de ida/volta, armazenamento e trabalho atribuído ao callsite do loop,
em vez de supor que todas as 330 vêm do teste source-Clean/revision.

## Chain−cut: 879 instruções exatas por nó

Flush atual: Chain **1209N+1528**, cut **330N+1565**. Diferencial:
**879N−37 instr/escrita**, antes de set (idêntico nos dois fixtures).

Contabilidade exclusiva, sem sobreposição:

| Bloco adicional na Chain | Instr/nó |
| --- | ---: |
| Completion typed/slots/guards e helpers graph, sem callback/comparador | **271** |
| Avaliador: avanço, Execute e retorno de callback | **206** |
| Read limpo: read_copy 118 + tracking 63 + notify 15 | **196** |
| reconcile | **121** |
| prepare lógico | **54** |
| Corpo da callback/adapter | **27** |
| Corpo de PartialEq | **4** |
| **Total Chain−cut** | **879** |

O 271 equivale a 223 exclusivos da função física typed execute + 48 dos
helpers graph emitidos separadamente. Nos agrupamentos por linha, parte
desses helpers aparece no callsite typed e parte no próprio graph; não somar
ambas as representações. Os blocos read/callback/comparador estão DENTRO dos
498 inclusivos atuais de typed execute, não em adição a eles.

O adicional de 206 no avaliador fecha por callsites:

| Callsite adicional | Instr/nó |
| --- | ---: |
| Execute: resolução/phase/descriptor/reconcile e kernels inlined | **80** |
| tick: retomada/pop de callback e kernels | **58** |
| next: preparar execução/Action após detectar mudança | **30** |
| Context::validation após callback | **16** |
| run: retorno de tick, chamada e epoch | **14** |
| pop: caminho pós-callback com revalidação | **8** |
| push | **0** |
| **Total** | **206** |

Não são visitas extras. Aumento de custo por caminho não prova que cada item
possa ser removido preservando contratos. Parte do trabalho de kernel está
inlined e só foi atribuída, não causalmente isolada.

Maiores callsites individuais positivos no diferencial:

| Linha/callsite normal | Instr/nó |
| --- | ---: |
| lib.rs:870, corpo do kernel de reconcile | **105** |
| lib.rs:687, kernel do read_copy limpo | **76** |
| lib.rs:1282, preflight freshness/estado de reads do memo | **64** |
| lib.rs:1301, caminho de comparação com guards | **21** |
| fused.rs:281, retorno de Action pronta | **18** |
| lib.rs:1249, revalidação após compute | **18** |
| lib.rs:1141, check da geração do reader no tracking | **17** |
| ownership.rs:433, depth/disposal preflight lógico | **14** |
| fused.rs:350, callsite de resolução antes de reconcile | **14** |

A lista completa contém também deltas negativos (por exemplo, menos caminho
Clean no retorno). O analisador soma TODOS os sinais para chegar a 879, sem
selecionar apenas as linhas que favorecem uma hipótese. O preflight de 64 é
contrato de primeira leitura/fault injection; esta atribuição não autoriza sua
remoção. Não foi implementada fusão adicional de retorno de callback.

## Session curta: o que o probe decidiu

Probe temporário com `#[inline(never)]` em update E Session::run, nos dois
avaliadores. Separar run permite medir entrada/saída sem incluir o corpo da
avaliação. Erros/cleanup não foram removidos. Os testes nativos typed com
all-features passaram nos dois modos; só esses quatro atributos diferem do
snapshot normal, e foram retirados após selar o probe.

Em Diamond e Fan-out, N=64/256/384, a contagem diferencial confirmou exatamente
**N Sessions por escrita**. Diamond inclui a Session externa e N−1 aninhadas;
Fan-out abre uma por efeito. O read Copy já limpo não abre uma Session.

| Entrada/saída excluindo run | Full | Demand |
| --- | ---: | ---: |
| Instr/Session (todas as escalas e os dois fixtures) | **57** | **99** |

No Demand, 99 se repartem em capture **15**, validação inicial **8**, restore
**17**, demais callsites de entrada/saída **58** e 1 sem atribuição de linha.
Os três blocos explícitos de Context somam **40**; o excesso total sobre Full
é **42**. É evidência de custo fixo relevante e de onde investigar uma entrada
mais leve para demandas aninhadas, não comprovação de que todos os 42 possam
ser retirados nem de que expliquem sozinhos o delta normal de +98/+69.

**O probe altera codegen**:

| Fixture | Full normal | Demand normal | Full probe | Demand probe |
| --- | ---: | ---: | ---: | ---: |
| Diamond, slope flush | 1572 | 1670 | 1631 | 1701 |
| Fan-out, slope flush | 2471,75 | 2540,75 | 2553,75 | 2568,75 |

Somar/subtrair os custos de entrada do probe aos totais normais como previsão
de ganho seria incorreto. Os inclusivos de run também se sobrepõem em Sessions
aninhadas: são excluídos de TODAS as invocações ao calcular 57/99, nunca somados
como decomposição disjunta do programa.

## Limites e reprodução

Não houve perf nativo ou controle de frequência nesta etapa. Instr/ns calculado
com slopes e tempos de outro executável é throughput implícito aproximado,
não IPC nem diagnóstico causal de loads dependentes/cache. As vitórias escalares
contra Sycamore em Diamond/Conditional não validam o desenho do walk.

```sh
python3 benches/profile-callsites.py NOVO_LABEL
python3 benches/analyze-callsites.py
```

O analisador verifica os labels arquivados `callsite-current` e
`callsite-entry-probe`, hashes de fontes/binários/traces, a diferença de apenas
quatro atributos, restauração da fonte normal, contabilidade por PC, fits,
duplicação exata de Ir por iteração e chamadas diferenciais. Correlaciona também
a fonte Rust com o timing atual, sem misturá-la com uma revisão anterior.

Dados completos sob `benches/.log/`: manifests, bundles, executáveis selados,
traces, `inline-stacks.{log,json}`, `disassembly.log`,
`instructions-MODO-FIXTURE-256.json` e `callsite-analysis.json`. Findings neste
arquivo são versionáveis; os dados brutos continuam ignorados. Sem promoção,
mudança de contratos, commit ou push.

Após remover os atributos temporários, passaram novamente workspace default
e all-features nos dois modos, Clippy all-targets/all-features `-D warnings`,
fmt e diff-check; logs/hashes em `callsite-validation.json`. Não foi alegado
novo Miri: o código Rust é o mesmo outlined já validado anteriormente, sem
mudança de gerações, depth, contexto ou fronteiras de callback nesta etapa.
