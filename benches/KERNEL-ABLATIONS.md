# Fusões locais: primeira rodada após a atribuição por callsite

> Checkpoint histórico. Fonte atual e investigação de todas as hipóteses:
> [HYPOTHESIS-MATRIX.md](HYPOTHESIS-MATRIX.md).

## Resultado mantido

Publicação typed em um kernel e reconcile estável em uma passada foram
medidos separadamente e só então combinados. Ambos permanecem no protótipo.
O prepare outlined foi preservado byte a byte em ownership.rs/fused.rs;
FullFrames continua default, produção não mudou. Sem commit/push.

Slopes de flush, instruções por unidade de N, profiling, abort efetivo:

| Fixture | Full outlined | Full final | Demand outlined | Demand final |
| --- | ---: | ---: | ---: | ---: |
| Chain | 1240 | **1210** | 1209 | **1179** |
| cut | 439 | **439** | 330 | **330** |
| Diamond | 1572 | **1537** | 1670 | **1635** |
| Fan-out | 2471,75 | **2424,75** | 2540,75 | **2493,75** |

N=memos em Chain/cut; branches em Diamond; pares memo/efeito em Fan-out.
Set não mudou: 58/58/95/122 instr/unidade respectivamente. Demand total
set+flush na Chain passa de 1267 para **1237**, não 1179. O custo linear do
cut continua aberto. A coleta nativa posterior contra referências está abaixo;
não é uma comparação causal antes/depois nem evidência de IPC, spills ou cache.

## Tempos atuais contra referências

`current-kernel-references.json`: fonte Rust idêntica a `kernel-final-directed`,
um executável bench-abort com `-C panic=abort` e smoke efetivo abort, CPU 2,
20 blocos simétricos, quatro amostras por modo, N=256. Médias em µs/op:

| Fixture | Produção | Full | Demand | Sycamore | Leptos |
| --- | ---: | ---: | ---: | ---: | ---: |
| Chain | 66,087 | 39,122 | 35,822 | 22,530 | 52,649 |
| cut | 20,414 | 16,397 | 11,893 | 5,091 | 22,226 |
| Diamond | 65,570 | 46,841 | 51,714 | 62,356 | 186,487 |
| Fan-out | 100,919 | 79,318 | 80,308 | 78,462 | 225,435 |
| Conditional | 77,445 | 63,867 | 67,195 | 96,231 | 332,104 |

Razões pareadas Demand/Sycamore: 1,590× / 2,336× / 0,829× / 1,023× / 0,698×,
nessa ordem. Fan-out variou 0,990×–1,057× entre ciclos: aproximadamente empate,
não vitória estabelecida. Estes tempos não foram misturados com os anteriores;
diferenças entre coletas não isolam o efeito das fusões. Sem isolamento físico
ou teste dedicado de significância, e sem equivalência completa de API/contratos.

## Ablações isoladas contra outlined

Deltas de slope flush, cada variante parte do MESMO controle, não da anterior:

| Variante | Modo | Chain | cut | Diamond | Fan-out |
| --- | --- | ---: | ---: | ---: | ---: |
| Publicação em um kernel | Full e Demand | **−16** | 0 | **−16** | **−19** |
| Reconcile em uma passada | Full e Demand | **−14** | 0 | **−19** | **−28** |
| Execução/reconcile, retorno bool | Full | +3 | 0 | +4 | +7 |
| Idem | Demand | −16 | −3 | −12 | −27 |
| Execução/reconcile, retorno unit | Full | +12 | 0 | +14 | +25 |
| Idem | Demand | −4 | −1 | 0 | −4 |
| Publicação + uma passada | Full e Demand | **−30** | **0** | **−35** | **−47** |

As duas formas de execução/reconcile NÃO foram mantidas: regressão concreta
do Full nas três formas com callbacks. Isso não reprova a ideia em geral nem
isola o ganho do acesso hot direto. A variante bool contém DUAS mudanças no
protocolo de execução: remove header_mut redundante dentro do mesmo kernel e
move o teste de vida pré-reconcile para dentro do reconcile. Não atribuir seus
efeitos a uma só delas. A variante unit muda apenas a forma do retorno externo
em relação à bool; também foi medida isoladamente contra o controle.

### Publicação

Após compute e comparador, um kernel revalida a Key completa, faz checked_add
da revisão, troca os slots e publica a revisão. Nenhum ponteiro emprestado cruza
callback. checked_add precede QUALQUER publicação. O valor antigo sai do kernel
antes de rt.retire: seu Drop continua autorizado a reentrar conforme o protocolo.
Alive/preflight anteriores ao comparador NÃO foram fundidos nesta etapa.

Na Chain, a atribuição exclusiva fecha exatamente em −16: typed execute −15;
os dois helpers graph antigos somavam 48 e foram substituídos por um de 47
(−1). O resto dos símbolos não mudou de slope. Logo não se confirmou a
estimativa de −60 a −100 para esta fusão PARTICULAR de publicação.

### Reconcile

Mantém o preflight de primeira leitura ANTES do comparador, sem reutilizar seu
resultado depois de callbacks/destructors. Dentro do reconcile, sources e reads
são percorridos uma vez com iter_mut().zip(), resolvendo hot por índice com
checagens explícitas de bounds, alive e geração, iguais às de header.

Cada par válido copia revision/freshness imediatamente. Se outro par divergir,
o fallback substitui sources por reads; unlink só consulta Key/backlink ordinal,
não as revisões já copiadas. Não há callback nem erro retornável entre a escrita
parcial e esse fallback. Fontes mortas, tombstones e ordem alterada continuam
no caminho genérico. Nenhum observer_index é copiado antecipadamente.

Na Chain, todo o delta exclusivo de −14 está no símbolo reconcile. A redução
de 40–60 estimada não se confirmou neste caminho estável de uma fonte.

## Validação e proveniência

Cada variante passou ANTES de sua coleta por workspace all-features nos dois
modos e Miri dirigido. Primeira publicação: **9 testes por modo**; demais:
**10 por modo**. Incluem disposal/reuso, provisional-source/reuso com troca de
tipo, falha de cleanup, stale-first-read, os quatro fused e novos testes:

- Overflow de revisão do memo preserva o índice/valor do slot atual e o slot
  alternativo vazio; descarta só o candidato, sem descartar o valor publicado.
- Prefixo atualizado seguido de cauda divergente mantém backlinks corretos.
  A versão final desse teste força um tracking_prefix otimista via fault
  injection, para realmente atravessar o primeiro par e depois cair no fallback;
  sem isso, o tracking normal já teria eliminado o caminho estável.

`kernel-final-directed` é a fonte final exata. `kernel-combined` é a primeira
combinação; `kernel-final` só formata publicação; `kernel-final-directed` reforça
o teste acima. Todos têm os mesmos slopes/interceptos. O reforço recebeu nova
validação e nova coleta, não aprovação emprestada de fonte anterior.

Cada label tem 48 processos Callgrind: 2 modos × 4 fixtures × 3 escalas
(64/256/384) × set/flush, 200 escritas. Fits exatos, resíduo <1e-6 instr/escrita.
Executáveis profiling selados e hashes dos bundles/traces; cada processo
confirma panic=abort, sem diagnostics/verification. Não são tempos release.

A fonte final também passou workspace default nos dois modos, Clippy
all-targets/all-features `-D warnings`, fmt e diff-check. Miri aqui é DIRIGIDO,
não todos os ports/modelo nem revisão unsafe independente.

Reprodução/checagem:

```sh
python3 benches/validate-kernel-ablation.py NOVO_LABEL
python3 benches/profile-walk-phases.py NOVO_LABEL --modes full topological \
  --shapes chain cut diamond fan_out
python3 benches/analyze-kernel-ablations.py
```

O analisador confere os labels arquivados, correspondência de fontes com os
gates, hashes dos binários/logs/traces, fits, soma dos custos exclusivos,
isolamento das duas mudanças mantidas e composição da fonte final. Brutos e
`kernel-analysis.json` ficam sob `benches/.log/`, ignorados; este documento é
versionável. Os analisadores antigos que exigem fonte outlined idêntica são
históricos e agora falham legitimamente na checagem da fonte atual.

## Próxima rodada, ainda não executada

- Separar header_mut redundante da mudança de fronteira pós-callback; investigar
  alive + preflight e carregar descriptor no caminho lógico preparado.
- Cut: medir ABI efetiva e ablar Result<Action, ()> e hoist de READ separadamente;
  usar o dump por PC/callsite já arquivado, sem tratar tick inteiro como dispatch.
- Corrigir adapter com nova medição, sem subtrair 27 por estimativa.
- Só depois decidir sobre descida reescrita ou update_dirty sem Frame.
