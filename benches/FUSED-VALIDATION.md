# Validação fundida: compactação sem promoção

> Extensão atual com Diamond/Fan-out e status final do Miri dirigido:
> [BRANCHING-COST.md](BRANCHING-COST.md). O analisador deste checkpoint exige
> também production ports Miri, que atingiram timeout; não houve aprovação
> integral de `analyze-fused.py`.

## Resultado

Só FullFrames (default) e DemandDriven permanecem; LightFrames foi removido.
`evaluation.rs` é byte a byte igual ao controle selado. DemandDriven foi
reescrito em `experiments/typed-runtime/src/fused.rs`, não adicionado como
outro executor. Nenhum contrato foi intencionalmente retirado.

Inclinações por escala (N=64/256/384), instruções por nó/escrita:

| Motor/revisão | set Chain/cut | flush Chain | flush cut | Chain−cut total |
| --- | ---: | ---: | ---: | ---: |
| Full controle | 58 | 1246 | 435 | 811 |
| Demand controle | 58 | 1294 | 376 | 918 |
| Full atual | 58 | 1251 | 439 | 812 |
| Demand atual | 58 | 1226 | 332 | 894 |
| Sycamore controle/atual | 744 / 111 | 0 | 0 | 633 |

**Sycamore executa dentro de set; settle/flush é vazio.** Compare set+flush,
não o flush isolado entre motores. Mesma construção de Chain/cut, counters de
callbacks verificados fora da região coletada, fronteiras noinline separadas.
Todas as coletas usam abort efetivo, sem diagnostics/verification. Profiling
não é evidência de tempo. O excesso no Chain não é só o walk: o delta de
execução é 894 no Demand atual contra 633 no Sycamore, sem atribuição causal
exata por subtração de fixtures.

O frame Demand caiu de 64 para **32 B** em 64 bits; o Full permanece em 136 B.
Uma cadeia de 256 frames ocupa 8 KiB no Demand contra 34 KiB no Full, sem contar
capacidade excedente/Context. Cursor continua usize, sem novo limite u32.
A redução de 376→332 é **11,7% em instruções**, não o objetivo de ~150.

## Ablações e tempo

1. `fused-control`: mesmas fronteiras para Full/Demand/Sycamore, código anterior.
   `fused-timing-control`: executável abort separado, selado para ABBA nativo.
2. `fused-stage-a`: Context por sessão, frame compacto, protocolo de callback do
   Full e pop/retomada fundidos, mas checagens de geração repetidas. Unificar
   também o Full regrediu fixtures (Diamond +9,45%); essa parte foi rejeitada.
   Demand: slopes cut 356, Chain 1235.
3. `fused-stage-b`: Full restaurado; Demand com certificados de região sem
   callback. Slopes 332/1226. A comparação A→B inclui restauração do Full e
   efeitos de codegen/layout: não é atribuição causal exclusiva às gerações.

ABBA (`abba` duas vezes, quatro amostras por executável/modo, CPU 2), abort
verificado no benchmark; valores médios em µs, antes→depois do Demand:

| Fixture | Antes | Depois | Razão pareada |
| --- | ---: | ---: | ---: |
| cut | 12,437 | 11,753 | −5,50% |
| Chain | 39,227 | 37,218 | −5,13% |
| Diamond | 56,441 | 52,788 | −6,45% |
| Fan-out | 87,307 | 82,630 | −5,36% |
| Conditional | 72,674 | 69,582 | −4,26% |
| writable | 1,764 | 1,675 | −5,06% |
| owners | 3,651 | 3,692 | +1,15% |

Full atual no mesmo experimento: cut 16,068; Chain 38,648; Diamond 48,647;
Fan-out 79,790; Conditional 65,131; writable 1,579; owners 3,650.
Demand continua perdendo em Diamond/Fan-out/Conditional/contratos. **Sem
promoção**. Full byte-idêntico teve slopes ligeiramente diferentes no binário
novo; não atribuir todo delta ao algoritmo. Não há IC de superioridade nem
isolamento físico; não há nova comparação nativa com Sycamore.

## O que step faz

No cut estável, ignorando M1/efeito/custo fixo, há duas chamadas de step por
memo: descer e retornar. No controle, a contagem em nível de fonte é:

| Caminho | chamadas header/header_mut |
| --- | ---: |
| push | 2 |
| step descida: leitor inicial + loop + fonte | 3 |
| step retorno: leitor inicial + observe(fonte/leitor) + loop + final | 5 |
| pop | 1 |
| total | **11** |

Cold `as_ref().unwrap()` roda duas vezes por memo validado nesse caminho.
São contagens lógicas do código, NÃO 11 branches de máquina: o otimizador
elimina/funde parte disso. A anotação DWARF de step em planning.rs+lib.rs
respondia por 41,1% self, não mede causalmente o custo dessas verificações.
O Demand atual evita revalidar o leitor entre callbacks, mas ainda resolve a
fonte na descida e no retorno e o leitor após o callback de M1. Isso reduz a
contagem lógica para ~3 checks no caminho de cut, não para zero. Reler Source
pelo ordinal substitui o antigo Option<Source>; bounds e Option continuam.

## Chain: atribuição inclusiva

No controle Demand/256, 200 escritas, flush = 66.576.000 instruções:

- planning::execute inclusivo: 41.407.600 (62,20%), ~809 instr/nó;
- typed execute u64: 26.060.800 (39,14%), ~509 instr/nó;
- read_copy u64: 10.639.800 (15,98%), ~208 instr/nó;
- reconcile: entradas DWARF de 6.219.400 (9,34%) e 3.957.800 (5,94%);
- prepare_execution: entradas de 3.906.400 (5,87%) e 1.850.400 (2,78%).

Entradas inclusivas SE SOBREPÕEM; não somá-las como decomposição disjunta.
Callbacks/get incluem update limpo, tracking e resolução tipada. O comparador
atual é Equality<T> sobre slots tipados: **não tem 2 downcasts/2 try_borrow**.
A publicação atual também **não chama mark_observers por memo**; marking foi
transitivo em set, com promoção Dirty imediata retirada. A comparação e a
publicação continuam dentro do typed execute; não foi isolado seu custo
completo de funções inlined. As anotações inclusivas e por linha estão nos
diretórios ignorados dos controles e da revisão B.

## Segurança e reprodução

Passaram as suítes workspace all-features e default nos dois modos: por modo,
all-features 246 testes de produção +171 typed, 51 integrações e 20 doctests;
default 240+156, 49 integrações e 20 doctests. Clippy all-targets/all-features
com `-D warnings`, fmt e diff-check também passaram.

`UNSAFE-AUDIT.md` descreve o certificado local e suas barreiras. Índices seguem
checados; não há referência de grafo/frame/Source atravessando callback.
Erro/unwind sempre revalidam no pop. Certificados só evitam resolução repetida
do mesmo Key dentro de uma região comprovadamente sem código de usuário.

Raw logs, executáveis selados, bundles e hashes ficam sob `benches/.log/`:
`fused-{control,stage-a,stage-b}.json`, `fused-stage-{a,b}-abba.json`,
`fused-b-validation.json` e `fused-b-miri-*.json`. Relatórios anteriores são
históricos, não validação da revisão B. Não houve commit/push ou mudança do
runtime de produção.

```sh
python3 benches/profile-walk-phases.py NOVO_LABEL
python3 benches/compare-local-checkpoint.py --label NOVO_ABBA \
  --baseline benches/.log/fused-timing-control.json
TYPED_EVALUATION=topological cargo test --locked --profile fast-dev --workspace --all-features
TYPED_EVALUATION=full python3 benches/run-typed-miri.py NOVO_MIRI --case fused
python3 benches/analyze-fused.py
```

Miri dirigido não equivale a revisão unsafe independente ou prova global de
segurança. Modelo completo/all-ports sob todos os modos e Miri ainda requerem
evidência própria; os testes nativos não substituem isso.
