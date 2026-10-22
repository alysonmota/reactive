# Plano de ação: redesenho orientado a desempenho

## Decisão

Parar a sequência de microfusões como estratégia principal. Construir um
núcleo alternativo, comparável ao atual, com novas representações de storage,
valores, execução e ownership. `unsafe` está autorizado; remover contratos ou
expor uma API safe insound não é uma otimização aceitável.

Este documento é um plano, não uma implementação nem promessa de ganho.
O runtime atual permanece como oráculo funcional e baseline congelado.

## 1. Meta e tamanho do problema

Objetivo final: tempo e memória iguais ou melhores que as referências nos
workloads equivalentes, não só melhorar em relação a nós mesmos.

Última comparação completa preservada, Chain/256: reactive 67.513 µs,
Sycamore 23.935 µs e Leptos 59.833 µs. São dados históricos, não o baseline da
próxima rodada. Naquela medição, alcançar Sycamore exigiria cortar **~64.5% do
nosso tempo**, não 5–10%. A ablação combinada de checks deu ~17.8% em outro
experimento: evidência de custo, não solução para toda a distância.

Critérios finais, reportados separadamente:

1. **Tempo:** razão local/melhor referência <= 1 em cada caso primário
   semanticamente equivalente; médias geométricas por família para resumir,
   nunca para esconder perdas em casos individuais.
2. **Memória:** bytes vivos/allocs de construção e pico solicitados <= melhor
   referência medida para o mesmo grafo e lifetime. RSS em relatório separado.
3. **Steady state:** zero alloc/realloc em grafos escalares estáveis, incluindo
   pilha de avaliação após warmup. Memória retida pelo runtime entra no total.
4. **Correção:** conformance/modelo completos; testes dirigidos de unsafe.

Se o contrato de uma referência for diferente, registrar a diferença e manter
o caso fora da declaração de equivalência, não adaptar silenciosamente o teste.
Não há garantia de que todas essas metas sejam simultaneamente atingíveis.

## 2. Sequência executável

| Etapa | Entrega | Pergunta respondida | Gate proposto |
|---|---|---|---|
| P0 | Laboratório confiável + baseline congelado | A execução mede o binário certo? | Sham, hashes e replay conferem |
| P1 | Storage quente/frio + kernels de acesso | Quanto custa a representação atual? | Menos bytes quentes e instruções/acesso |
| P2 | Storage tipado e caminho de leitura/publicação | Podemos retirar Any/Rc/RefCell do caminho escalar? | Ganho em reads e atualização completa |
| P3 | Avaliador com frames e sessão de execução | Podemos parar de reinstalar contexto completo por Check? | Chain completo e cut melhoram juntos |
| P4 | Execution scopes virtuais/lazy | Podemos não criar/renovar escopo vazio físico? | Menos owners/memória, recurso real não regride |
| P5 | Backend de edges + propagação/scheduler | Podemos reduzir indirections e trabalho por edge? | Melhora em fan-out/dynamic, sem perder cut |
| P6 | Integração, tuning e validação final | O conjunto bate as referências? | Matriz final de tempo/memória/conformance |

P1/P2 formam o primeiro incremento integrado. P3/P4 vêm depois; P5 só começa
com perfil novo. Não implementar todos os subsistemas antes de medir.

### P0 — Um laboratório que não produz falsos ganhos

- Preservar baseline atual e fontes exatas das referências pinadas. O clone
  Leptos em references pode ter arquitetura diferente da versão do benchmark;
  registrar isso em qualquer decisão tirada dele.
- Criar pacote/harness de pesquisa separado, sem bypasses no build normal.
  Mesmos fixtures devem dirigir baseline, novo núcleo e referências.
- Identificar cada executável por hash de fontes, flags, toolchain **e binário**.
  Target exclusivo por variante, ou clean explícito do pacote ao trocar fontes.
  Não reutilizar as séries invalidadas da ablação.
- Rodar sham e smoke funcional antes de benchmark. Compilar tudo antes de medir.
- Driver com timeout por etapa, progresso, checkpoint e seleção de um candidato.
  Piloto curto antes de reservar uma bateria completa; falhas não reiniciam tudo.
- Adicionar traces de operações observáveis para comparar o novo núcleo com o
  atual: outputs, ordem dos effects/cleanups, erros e drops.

**Saída:** baseline novo dos 45 casos, Chain/cut e recursos; JSON com tempo,
allocator, trabalho e instruções/cache, obtidos em execuções separadas.

### P1 — Separar grafo quente, payload e metadados frios

Arquivos atuais de referência: `src/node.rs`, `src/arena.rs`, `src/runtime.rs`.

- Substituir Node monolítico por header quente e payload frio separado.
  Header: estado/fase, geração/vida, revisão relevante e acesso a edges.
  Ownership, erros, callbacks, dedup grande e dados raros ficam fora dele.
- Prototipar dois layouts pequenos: headers contíguos indexados e slots em
  páginas estáveis. Medir locality, overhead de tabela de páginas e memória;
  escolher um, não conservar dois backends permanentes sem benefício.
- Alvo exploratório: header <= 64 bytes no alvo 64-bit. Não é prova de ganho
  nem motivo para truncar revisões/gerações ou provocar wrap.
- Introduzir kernels internos de acesso indexado via UnsafeCell/ponteiros,
  com operações pequenas e contratos de exclusividade escritos por operação.
  Validar geração na entrada; não criar `&mut Storage` amplo durante callbacks.
- Ponteiro estável não significa objeto vivo: free/reuse ainda exige validação
  ou pin/lease de execução. Nenhum ponteiro interno vira handle público direto.

**Testes:** modelo de arena, crescimento de páginas durante callback, reuse,
disposal/recriação do mesmo índice, exaustão geracional, alinhamento e ZST.
**Medição:** tamanho/layout, bytes por grafo, clean reads, scans de sources e
instruções/cache misses por atualização. Integrar só com ganho verificável.

### P2 — Remover Any e refcounts do caminho tipado

Referências atuais: `src/node.rs`, `src/handle.rs`, `src/runtime/signals.rs`,
`src/runtime/memos.rs`.

- Nó tipado alocado uma vez; descriptor/vtable interno com operações
  monomorfizadas de compute, igualdade, leitura e drop.
- Handle público continua não proprietário/geracional. Resolução valida
  identidade, slot e descriptor; extração tipada usa cast auditado, não várias
  consultas Any ao mesmo objeto em cada leitura/publicação.
- Signal possui T diretamente; memo possui storage alinhado para valor atual e
  candidato reutilizáveis. Considerar MaybeUninit com bits explícitos de
  inicialização. Evitar Rc por acesso e a cadeia Rc<dyn Any> -> RefCell<Option<T>>.
- Descriptor é imutável após criação; resultado de compute é escrito no slot
  tipado esperado, em vez de retornar um Value arbitrário. Essa mudança de
  representação permite eliminar verificações redundantes por construção.
- Borrow de valor precisa de estado explícito para try_with/Clone/Drop e APIs
  reentrantes. UnsafeCell não autoriza ler T enquanto ele é mutado/destruído.
- Publicação só troca slots após igualdade/revisões/reconciliação válidas;
  retirar T antes de Drop e destruir fora de acessos exclusivos ao grafo.

**Testes:** Copy, não-Clone, ZST, alinhamento alto, Rc interno, Drop reentrante,
Clone/equals que panica, igualdade preservando o valor antigo, falha antes/depois
da escrita do candidato, disposal do target e falhas de tipo/geração.
**Medição:** clean reads, write igual/alterado, Chain, recursos, construção/drop.
Não aceitar um ganho escalar que esconda vazamento ou custo excessivo em T grande.

**Checkpoint P1+P2:** alvo de pesquisa >=25% menos tempo em Chain completo e
redução da memória persistente contra o baseline novo. Se não aparecer após
duas versões integradas, perf/assembly devem identificar o custo restante antes
de iniciar outro grande subsistema. Não é um gate para relaxar correção.

### P3 — Sessão de avaliação + frames reutilizáveis

Referências atuais: `src/runtime.rs`, `src/context.rs`, `src/runtime/memos.rs`.

- Separar navegação interna sem user-code das fronteiras compute/equals/
  cleanup/Drop/hook. Checking usa frames leves, não guards completos por nó.
- Sessão de avaliação conserva estado interno necessário; capabilities e
  contexto de callback continuam cumulativos, inclusive entre runtimes.
  Não presumir que toda reentrada pertence à sessão externa.
- Frames {ID, cursor, revisão aguardada, etapa}; storage reutilizável da sessão
  com checkpoints de nesting e unwind LIFO. Sem Vec local alocado por update.
- Usar índices/offsets, não referências a frames através de callbacks: reentrada
  pode crescer/reorganizar a pilha. Fallback de capacidade faz parte da memória.
- Revalidar estado após fronteiras que podem executar usuário. Leases/epochs
  podem reduzir checks apenas se cobrirem **todos** os invalidadores. Callbacks
  podem causar poison mesmo quando não conseguem dispor o nó diretamente.
- Protótipo inicial mantém limite lógico 512; stack nativa não vira limite de
  correção. Aumentar/remover o limite é decisão de contrato separada.

Não repetir o protótipo iterativo descartado: ele mantinha guards completos,
revalidava a cada passo e alocava/realocava Vec por atualização.

**Testes:** chain/cut, diamond/reconvergência, múltiplas fontes, mudança dinâmica,
erro no frame mais profundo, nested reads, callbacks que alocam, reentrada de
outro runtime, panic durante cleanup/compare/Drop e retomada de effects.
**Gate:** zero alloc recorrente; ganho consistente em completo **e** cut, e
traces equivalentes. Não chamar de melhoria se só deslocar trabalho para write.

### P4 — Scopes virtuais, materializados apenas quando observáveis

Referência atual: `src/runtime/ownership.rs`.

- Separar identidade lógica da execução de um Owner físico com três Vecs.
  Execuções vazias avançam epoch em metadado compacto, sem renovar arena/parent.
- Materializar scope físico ao consultar current_owner, anexar recurso, criar
  filho/nó ou registrar cleanup. O handle materializado deve invalidar no
  término lógico da execução anterior, mesmo se o slot físico for reutilizado.
- current_owner não pode retornar o life owner como atalho: isso prolongaria
  recursos e quebraria observabilidade. A consulta também precisa considerar
  falha de materialização sem conceder Create ao callback read-only.
- Caminho com recursos reais faz teardown completo, ordenado e protegido;
  capturas, explicit owners, shutdown e overflow continuam corretos.

**Testes:** nenhum recurso; consulta current_owner sem recursos; host anexa
recurso depois do compute; nó/filho/cleanup; cleanup dispõe o memo; scopes
antigos materializados/não materializados; epoch esgotado; panic e shutdown.
**Gate:** menos owners/bytes físicos nos grafos vazios e ganho de atualização;
trabalho/custo com recursos exposto separadamente. É redesenho de representação,
não a fusão de três chamadas descartada no experimento anterior.

### P5 — Edges e algoritmo após novo perfil

Referências atuais: `src/runtime/graph.rs`, `src/runtime/effects.rs` e fixtures.

- Medir por edge: marcações, sources verificados, reconciliações, cache misses.
  Separar overhead de representação de excesso de trabalho lógico.
- Comparar baseline de buffers indexados com um slab de links bidirecionais
  compactos. Cada link pode guardar revisão observada e backlinks; comparar
  locality/deletion contra overhead de ponteiros, não assumir lista mais rápida.
- Reutilizar prefixo estável sem read log duplicado quando comprovado; fallback
  cobre reorder/duplicação/branches. Mutação durante reentrada não pode invalidar
  cursor silenciosamente; links também precisam de vida/epoch.
- Propagar apenas transições relevantes de estado numa wave. Avaliar stamps
  para evitar revisitas duplicadas em reconvergência, sem perder revisão fresca
  após writes intercalados, batch ou equality cut.
- Pré-computar chave de ordem dos effects no enqueue quando permitido; preservar
  depth, sequência, duas filas e efeitos criados/reagendados durante flush.
- Eager/topológico é candidato separado só se o perfil justificar. Não alterar
  lazy silenciosamente para ganhar no benchmark de grafos totalmente observados.

**Testes:** fan-in/fan-out, diamond profundo, reconvergência alta, fontes
descartadas, dependências alternadas, leituras duplicadas, batches multiwrite,
subgrafo não observado, orçamento/round limit e ordem exata dos effects.

### P6 — Integração e decisão

- Rodar os 45 casos de referência + matriz de recursos e workloads adversariais.
  Revalidar toda a combinação: ganhos de etapas não são aditivos.
- Medir build time, construção, atualização, disposal/shutdown, memória viva,
  pico e capacidades retidas; não transferir custo para fora do timer ocultamente.
- Perf/stat/assembly no núcleo integrado; especializar dispatch/hot paths só
  depois. LTO/codegen-units/PGO são controles separados e iguais nas referências
  quando aplicáveis; não substituir arquitetura por flags favoráveis só a nós.
- Auditar cada bloco unsafe e publicar contrato de aliasing/vida do módulo.
- Promover o novo backend somente quando passar os gates. Migrar API/docs depois;
  retirar o backend antigo apenas após equivalência e decisão explícita.

## 3. Política de medição e abandono

1. Piloto funcional + um ABBA curto por hipótese, sem série longa automática.
2. Candidato promissor: dois ciclos ABBA completos, referências intercaladas,
   sham, memória/trabalho e perf separados. Confirmar vencedor em nova sessão;
   se possível, segunda máquina/arquitetura antes de afirmar superioridade geral.
3. Se piloto é inconclusivo, usar contadores/assembly e no máximo duas revisões
   por hipótese antes de registrar e mudar de alvo. Evitar semanas de ruído.
4. Gates por incremento integrado, não por helper isolado. Pode-se aceitar
   regressão temporária documentada num protótipo, mas não escondê-la no produto.
5. Diferenças pequenas só contam com consistência e controle ambiental; não
   reprovar um redesenho de grande benefício por 1% ruidoso num microcaso.
6. Ao faltar o desempenho final, apresentar o gap por família e próximo gargalo.
   Não redefinir sucesso como “melhorou nosso baseline”.

## 4. Contratos e testes de unsafe

Preservar por padrão: lazy/equality cut, freshness, handles inválidos retornando
erro, ownership, ordem dos effects, capabilities, poison e teardown. Unsafe
serve para provar invariantes uma vez e acessá-los eficientemente, não ignorá-los.

- Nenhum `&mut`/`&T` interno vivo através de user-code que possa invalidá-lo.
- Geração válida não prova tipo nem exclusividade. Ponteiro estável não prova
  lifetime. Pinning de páginas não torna Vec interno estável.
- Não usar confiança em callbacks de usuário como precondição oculta de API safe.
- Miri dirigido por subsistema desde P1, não só no final. Sem testes UB intencionais.
- Modelo/differential fuzz com seeds versionadas e replay: operações válidas,
  IDs antigos, disposal, panic/erros injetados, branches e efeitos encadeados.
- Conformance completa e compile-fail em cada incremento promovido; sanitizer
  de endereço como complemento, não substituto de Miri/prova de aliasing.

Se preservar um contrato impedir a meta, produzir uma proposta separada com
ganho medido, cenários quebrados e migração (por exemplo, modo scoped que proíbe
disposal durante avaliação ou modo eager explícito). Mudanças dessas dependem
de aprovação; autorizar unsafe não é autorizar UB ou mudança semântica silenciosa.

## 5. Próxima unidade de trabalho

**Executar P0 e o protótipo integrado P1+P2.** Entrega mínima: mesmo fixture de
Chain/Diamond/Conditional dirigido pelos dois runtimes; headers quente/frio e
células tipadas sem Any/Rc por leitura; benchmarks contra referências, memória
e testes de panic/drop/reuse. Não iniciar outra rodada de ablação de checks.

Fontes para decisões: `references/sycamore-reactive/src/{node,memos,signals,root}.rs`,
`references/reactively/Reactive-algorithms.md` e código em `references/reactively/packages`,
`references/leptos/reactive_graph/src/{graph,computed,owner,signal}`. Extrair ideias,
registrar versão/licença e diferenças de contrato; não presumir equivalência
com os pacotes pinados de benchmark nem adotar claims de desempenho sem medir.
