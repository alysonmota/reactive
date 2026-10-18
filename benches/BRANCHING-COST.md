# Diamond/Fan-out: regressão de instruções no protocolo de avaliação

> Checkpoint anterior às ablações de Drop/tracking. Resultado e validação
> atuais em [SESSION-COST.md](SESSION-COST.md).

## Resultado

Nenhuma mudança de runtime nesta etapa. FullFrames continua default. Mesma
revisão de `experiments/typed-runtime/src/*.rs` da ablação B, novo harness
estendido para delimitar set/flush nos fixtures Diamond e Fan-out existentes.

Slopes medidos em N=64/256/384, instruções por unidade de escala/escrita:

| Fixture | set (ambos) | Full flush | Demand flush | Delta flush | Delta total |
| --- | ---: | ---: | ---: | ---: | ---: |
| Chain | 58 | 1251 | 1226 | −2,00% | −1,91% |
| Cut | 58 | 439 | 332 | −24,37% | −21,53% |
| Diamond | 95 | 1582 | 1703 | **+7,65%** | **+7,22%** |
| Fan-out | 122 | 2494,75 | 2592,75 | **+3,93%** | **+3,75%** |

N é número de memos na Chain/cut; branches no Diamond (N+1 memos e 1 efeito);
pares memo/efeito no Fan-out (N memos e N efeitos). Não confundir esse
denominador com total de nós ou total de callbacks. Todos os ajustes são
lineares nos três pontos, resíduo máximo <1e-6 instruções/escrita. O decimal
no Fan-out é real no trabalho amortizado, não arredondamento do ajuste.

O excesso de instruções explica a escala da regressão nativa anterior:
Diamond +8,5%, Fan-out +3,6%. Não é o caso de instruções iguais com mais tempo.
Não foram coletados novos contadores nativos perf/IPC/branch/L1. Portanto não
há atribuição causal de IPC, misses, latência ou cadeia de loads. Compactar
frames junto com outras mudanças também não foi uma ablação exclusiva de L1.
Para o ganho de tempo de compactação/protocolo, use o ABBA controlado anterior
(cut 12,437→11,753 µs, −5,50%), não 12,2 de outra rodada como controle pareado.

## Onde estão as instruções extras

Análise exclusiva por função real do arquivo Callgrind, N=256, por escrita/N:

| Componente | Diamond Full | Diamond Demand | Delta | Fan Full | Fan Demand | Delta |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Runtime::update, incluindo versões recursivas | 259,824 | 436,109 | +176,285 | 621 | 834 | +213 |
| enter (Full) / push (Demand) | 110,855 | 83,602 | −27,254 | 219 | 160 | −59 |
| pop | 76,594 | 48,375 | −28,219 | 152 | 96 | −56 |
| **Soma do delta** | | | **+120,812** | | | **+98** |

Diamond +120,812 por branch em 256 inclui a diferença fixa; o slope é +121.
Fan-out +98 é também o slope. Todas as outras funções têm o MESMO custo
exclusivo entre modos, incluindo execute, prepare_execution, read_copy,
record_read, reconcile e comparador. `Runtime::update` inclui o loop/tick,
execução inlined, Context/TLS e gerenciamento da Session, NÃO só a entrada.
Isso localiza o excesso no avaliador, mas não separa causalmente cada campo
de Context ou cada branch inlined.

Em Diamond, após a primeira mudança a callback do agregador demanda os outros
branches em sessões aninhadas curtas. Fan-out inicia uma Session por efeito,
com seu memo. A Chain amortiza a Session em uma pilha longa. A implementação
Demand já salva Context UMA vez por Session, não por callback. Já funde o pop
de callback com a retomada do tick. Reintroduzir essa mesma proposta não é uma
nova otimização: o próximo alvo é custo fixo por sessão curta e seus kernels,
sem perder revalidação, cleanup, caps/pure, profundidade ou lazy gating.

## Chain: ranking atual por memo

Demand, flush Chain/256, 200 escritas, 63.083.800 instruções coletadas.
Ranking de funções reais, incluindo todas as linhas/arquivos atribuídos à
mesma função (instr/memo/escrita, custo fixo incluído):

| Função | Inclusivo | Exclusivo |
| --- | ---: | ---: |
| typed execute u64 | **509,000** | 223,000 |
| read_copy u64 | **207,809** | 115,449 |
| reconcile | **121,473** | 121,473 |
| record_read | **77,301** | 77,301 |
| prepare_execution | **76,297** | 76,297 |
| corpo de PartialEq u64 | **4,000** | 4,000 |

**Não somar inclusivos**: read_copy/record_read estão dentro do compute/execute.
prepare/reconcile ficam fora de typed execute, no protocolo do avaliador.
O corpo de comparação custa 4, mas isso NÃO mede todo o wrapper/comparação:
dispatch, caps/ReaderRuntime e validação de slots estão inlined em execute.
Publicação/troca dos slots também está no exclusivo de execute; não foi
isolada como custo causal independente nesta coleta.

O código atual (`lib.rs:1224` em diante) toma/restaura o compute, revalida a
alocação depois de código de usuário, verifica freshness dos reads de memo,
compara sob guards, publica no slot alternado, incrementa revision e retira o
valor antigo. Não faz os downcasts/try_borrow/Rc clones sugeridos. Publicação
também não remarca observers por memo: essa promoção imediata foi retirada.
prepare_execution conserva o renew do scope lógico/cleanup; record_read é
parte mensurável do read limpo. Esses símbolos têm custos iguais no Full.

O ranking usa `fn`/`cfn` e os custos de chamadas do arquivo bruto. Soma dos
custos exclusivos de TODAS as funções deve ser exatamente o summary do trace.
Linhas inclusivas file:function de callgrind_annotate podem repetir atribuições
DWARF e se sobrepor; somá-las produziria ranking incorreto. Anotações com
`--inclusive=yes` e por linha continuam arquivadas para inspeção.

## Evidência e reprodução

**48 processos Callgrind** = 2 modos × 4 fixtures × 3 escalas × 2 fases.
O binário selado usa `profiling`, `RUSTFLAGS='-C panic=abort'`, sem diagnostics
ou verification; cada processo verifica `cfg!(panic="abort")` no executável.
O harness usa os adapters e Graph de comparison.rs sem alterar o formato das
callbacks dos fixtures. Preflight contado verifica outputs/effects fora das
regiões medidas. Setup, warmup, loop, output checks e teardown não são coletados.
Chain/cut reproduzem exatamente os slopes da ablação B no novo binário.

```sh
python3 benches/profile-walk-phases.py NOVO_LABEL --modes full topological \
  --shapes chain cut diamond fan_out
python3 benches/analyze-branching.py
```

O analisador verifica o label arquivado `branching-current`: hashes da fonte,
binário e traces, panic/features/fronteiras, ajustes lineares, anotações e soma
exclusiva. Raw logs/bundles/binário/JSON ficam em `benches/.log/`, fora do Git.

### Limite da validação Miri anterior

Full: 4 testes fused, 4 diferenciais e 32 audit passaram; os novos testes fused
e diferenciais exercitam os dois modos internamente. Production ports em Full
atingiram o timeout de 600 s, sem aprovação da suíte completa. O driver anterior
parou antes dos jobs configurados para Demand; isso não significa que passaram.
Os jobs dirigidos Demand passaram separadamente nesta etapa: 4 fused,
4 diferenciais e 32 audit, com hashes inalterados. O novo analisador verifica
esses 40 testes em cada modo, não exige nem alega aprovação dos ports/modelo.
Miri não é revisão unsafe independente nem prova global; não houve nova
mudança de unsafe/runtime, promoção, commit ou push.
