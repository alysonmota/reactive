# TODO — Runtime de reatividade fine-grained

Fonte de verdade: [reactive.md](./reactive.md). Esta lista transforma a
especificação em tarefas e registra o progresso validado. Marcar uma feature
somente após implementar seu contrato e validar os testes associados.

A ordem das etapas segue a seção 15. Testes unitários e verificadores devem
acompanhar cada etapa, não aguardar a etapa de benchmarks. Itens opcionais,
experimentais ou futuros estão identificados e não bloqueiam a primeira versão
do núcleo, salvo quando necessários para cumprir seus contratos.

## Próxima frente: redesenho arquitetural para desempenho

Plano concreto: [performance-redesign.md](./performance-redesign.md).
Prioridade: P0 + P1/P2 integrados, não novas microfusões/ablações.

- [x] P0: congelar baseline, binários/referências e laboratório diferencial;
  45 casos de baseline, matriz compartilhada de 64, sham e ABBA primário.
  Evidência em `benches/.log/P1P2-PROTOTYPE.md`.
- [ ] P1: storage quente/frio e kernels internos auditados com unsafe.
- [ ] P2: valores tipados/vtable, sem Any/Rc/RefCell no caminho escalar comum.
- [ ] P3: sessão de avaliação e frames reutilizáveis, sem guard completo por Check.
- [ ] P4: scopes de execução virtuais/materialização lazy com identidade correta.
- [ ] P5: edges/propagação/scheduler guiados por perfil do novo núcleo.
- [ ] P6: integração, matriz contra referências, memória e auditoria de unsafe.

Protótipo integrado P1/P2 entregue em `experiments/typed-runtime`: headers
contíguos de 40 bytes, células tipadas e testes diferenciais/Miri. Owners,
cleanups, effects mutáveis, duas filas/budgets, batch, hooks e WeakRuntime
reintegrados e medidos. Gates de promoção P1/P2 continuam abertos: falta paridade
completa de APIs/erros/diagnósticos e a suíte integral de conformance/modelos.
O código de produção não foi substituído. Evidência atual:
`benches/.log/P1P2-CONTRACTS.md` (os números anteriores são históricos).

### Refatoração de valores: cópia, clone e empréstimo explícitos

Este contrato substitui a API e o armazenamento de snapshots universais
descritos nos registros históricos das etapas anteriores.

- [x] Planejar separação das leituras em `try_get_copy`, `try_get_cloned`,
  `try_with` e variantes `try_peek_*`, sem expor Ref/células internas.
- [x] Remover APIs ambíguas antigas e renomear update para `try_update_cloned`.
- [x] Célula estável por signal e dois slots reutilizáveis por memo: candidato
  separado do valor atual até igualdade, revisão e reconciliação concluírem.
- [x] Empréstimo e Clone bloqueiam reentrada reativa; callback de edição ocorre
  depois, sob Updating, com conflitos e disposal revalidados.
- [x] Retirar T antes de Drop, fora de borrows de valor/grafo, sob proteção;
  manter fallback de erasure, poison, equality cut e restauração após falha.
- [x] Migrar fixtures, exemplos, testes e documentação; compile-fail cobre
  referência escapando e leitura Copy de tipo não Copy.
- [x] Medir comparação e allocator separadamente; registrar custo de slots
  persistentes e não presumir ganho de tempo só por zerar alocações estáveis.

### Arquitetura de resolução de handles

- [x] Comparar resolução/tipos/gerações com Sycamore e medir baseline isolado.
- [x] Compactar SlotId e free list sem wrap; preflight de capacidade e
  destruição protegida de recursos rejeitados.
- [x] Copy direto sem Rc temporário e RuntimeStatus quente separado do erro.
- [x] Unsafe mínimo na extração de slot validado, com invariantes locais,
  modelo de transições e Miri (arena e leituras).
- [x] Registrar candidatos, controles seguros/anteriores e resultados em
  `docs/handle-architecture.md` e `benches/.log/HANDLE-ARCHITECTURE.md`.

### Medição dos próximos gargalos

- [x] Repetir fases Chain/256, perf normal/cut e allocator no código atual.
- [x] Decompor seis escritas isoladas: incondicional/igual/alterada, com/sem Drop.
- [x] Dois ciclos ABBA para cada candidato inline-reads e pilha vazia, targets
  exclusivos, controles de referências e patch experimental preservado.
- [x] Registrar atribuição parcial por símbolo/linha e limites de inlining em
  `benches/.log/BOTTLENECKS-CURRENT.md`; nenhum candidato adotado nesta etapa.

### Otimizações de Chain após investigação

- [x] Medir 12 grupos de proteções retirados em cópias isoladas, sham e checks
  combinados, com dois ABBA por série; excluir a rodada de binários antigos e
  repetir com rebuild explícito e hashes dos executáveis. Concluir dez pares
  negativos, Miri dirigido da ablação de borrows e allocator/work separados.
  Não adotar bypasses; cobertura e limitações em `benches/.log/PROTECTION-ABLATION.md`.

- [x] Prototipar fusão de validação/renovação de execution owner vazio/tracking/
  Executing sem callbacks; preservar fallback protegido de owners com recursos.
  Medir dois ABBA por série Chain, confirmação longa, Diamond/Conditional e
  owners com cleanup/nó/filho. Descartado: empate no Chain confirmado e custo
  em equality cut/fallback. Patches e 102 cenários allocator/work idênticos em
  `benches/.log/EMPTY-OWNER-EXPERIMENT.md` e `empty-owner-analysis.json`.

- [x] Investigar retorno da última fonte sem nova resolução, scopes de Checking
  separado/compacto e protótipo de frames iterativos demand-driven, com ABBA de
  atualização/cut e Diamond/Conditional multissource/dinâmicos.
- [x] Descartar os quatro candidatos medidos; preservar patches e allocator do
  protótipo em `benches/.log/ASCENT-EXPERIMENTS.md` e `ascent-analysis.json`.
  Manter código de produção anterior e regressão de unwind de scopes suspensos.

- [x] Medir separadamente conclusão de guard e entrada Checking fundida, dois
  ciclos ABBA cada: descartar conclusão antecipada, adotar entrada fundida.
- [x] Confirmar entrada nos 45 casos em mais dois ciclos ABBA, regressões de
  ciclo/clean/depth/contexto, Miri dirigido e allocator sem mudança de trabalho.
  Evidência: `benches/.log/CHECK-OPTIMIZATION.md`.

- [x] Medir transições/guards, owners vazios, ascensão Check e snapshots
  separadamente; descartar fusão de start_tracking e Context de Cells separados.
- [x] Guard de avaliação reduzido, renovação vazia sem ContextGuard de disposal,
  janela Check sem callbacks reutilizando borrow e comparação de revisão sem
  nova resolução mutável do reader.
- [x] Transferir snapshot antigo para execução, restaurar identidade em igualdade
  e cache após falha; classificação estática de tipo para release sem Drop com
  fallback protegido por TypeId. Release especializado de signals descartado.
- [x] Registrar controles com diretório de build exclusivo, regressões e
  limitações em `benches/.log/CHAIN-OPTIMIZATION.md`; não inferir melhora de todos
  os workloads nem redução das alocações de snapshots.

## 1. Fundação: crate, runtime, handles e owners mínimos

Referências: seções 1, 2, 3, 8 e 9.

- [x] Estruturar um crate Rust independente, utilizável e testável sem janela,
  GUI, GPU, event loop ou scheduler obrigatório.
- [x] Implementar `Runtime::new()` e a fachada clonável `Rc<RuntimeInner>` com
  interior mutability segura e acesso explícito ao runtime.
- [x] Garantir implementação single-thread, sem acesso concorrente ao grafo.
- [x] Implementar identidade de runtime única no processo, sem reutilização ou
  wrap; falhar ao esgotá-la sem afetar runtimes existentes.
- [x] Implementar slots geracionais e lista de slots livres para nós e owners.
- [x] Incrementar geração na reutilização e aposentar slots quando esgotada.
- [x] Implementar `Signal<T>`, `Memo<T>`, `EffectHandle` e `OwnerHandle` com
  identidade do runtime, índice do slot e geração.
- [x] Implementar handles pequenos `Copy`/`Clone` sem exigir `T: Copy/Clone`,
  sem ponteiros dereferenciáveis ou registro global para localizar o runtime.
- [x] Impedir `Send`/`Sync` dos handles e do acesso ao runtime single-thread.
- [x] Validar identidade, índice, geração, vida e tipo nos acessos type-erased;
  retornar erro explícito, nunca valor default ou acesso ao ocupante novo.
- [x] Garantir que copiar handles não prolonga a vida dos nós.
- [x] Implementar `create_root(...)` e `create_owner(...)` com ownership explícito.
- [x] Rejeitar criação de nós sem owner; não introduzir owner global imortal.
- [x] Implementar vida `Alive`/`Disposing` e depth armazenado por owner, sem
  reparenting na API inicial.
- [x] Implementar `State` com `#[repr(u8)]`, `Clean = 0`, `Check = 1`, `Dirty = 2`.
- [x] Separar validade, inicialização, fase de avaliação, pertença a fila,
  revisões, rodada e falha; não deduzir scheduling apenas de `State`.
- [x] Implementar fases `Idle`, `Checking`, `Cleaning` e `Executing`, mantendo
  guard de avaliação ativo em qualquer fase não idle.
- [x] Implementar guards para restaurar tracking, owner, batching, capacidades
  e flags de avaliação em retorno normal, erro, saída antecipada ou unwind.
- [x] Adotar valores/closures armazenados `'static` e snapshots `Rc<T>`.
- [x] Passar `&Runtime` às closures e documentar como evitar ciclos de retenção
  por captura forte; usar handles ou fachada fraca quando apropriado.
- [x] Definir `RuntimeError` para handles/runtime inválidos, owner ausente,
  conflitos, ciclos, profundidade, operações proibidas, reentrância e falhas.
- [x] Implementar `Healthy`/`Poisoned(first_failure)` e teardown estrutural
  básico desde esta etapa; detalhar o protocolo completo na etapa 5.

Fundação disponível em `src/lib.rs`: `Runtime::try_new/new`,
`create_root()`, `create_owner(parent)`, `with_owner(owner, callback)`,
`dispose(owner_or_effect)`, `shutdown()` e `WeakRuntime`. Armazenamento de nós,
snapshots, metadata e guards são internos e testados; operações de signals
estão na etapa 2; memos/grafo estão na etapa 3 e filas/flush na etapa 4.
Cleanups e ownership de execução estão na etapa 5. Os guards desta
etapa restauram os contextos básicos, não implementam o tracking dinâmico.

## 2. Signals, igualdade, snapshots e escrita

Referências: seções 2.1, 3.1, 3.2, 4.1, 4.4 e 9.3.

- [x] Implementar `signal(initial)` com igualdade padrão para tipos apropriados.
- [x] Implementar `signal_with(initial, comparator)` com comparador `&T, &T`.
- [x] Manter signals vivos sempre `Clean`, sem sources, com observers.
- [x] Implementar `try_get(&runtime)` rastreado e `try_peek(&runtime)` sem
  tracking, devolvendo snapshot do último write imediatamente.
- [x] Implementar `try_set(&runtime, value)` com igualdade antes da publicação.
- [x] Em set igual, manter snapshot/revisão e não invalidar nem solicitar trabalho.
- [x] Executar comparadores sob guard de pureza, sem tracking ou borrow interno;
  revalidar o slot antes de publicar.
- [x] Publicar writes diferentes imediatamente e concluir marcação antes de
  liberar valores retirados; nunca executar effects dentro de `set`.
- [x] Implementar revisão do valor/freshness de signals, avançando apenas em
  publicação considerada diferente.
- [x] Implementar `try_update` para `T: Clone` sobre cópia de trabalho, sem
  referências mutáveis ao valor armazenado no grafo.
- [x] Capturar snapshot/revisão, executar `T::clone()` e callback fora dos
  borrows, e revalidar vida/revisão antes de comparar ou publicar.
- [x] Retornar `UpdateConflict` para mudança reentrante e `InvalidHandle` para
  disposal; não sobrescrever alteração interveniente ou repetir callback.
- [x] Documentar que update não reverte efeitos externos e que snapshots novos
  com `try_set` são preferíveis à cópia integral de coleções grandes.
- [x] Aplicar as permissões de `Updating` ao clone e à callback de update.
- [x] Permitir políticas de igualdade por identidade/revisão para snapshots,
  sem igualdade geométrica ou tolerância implícita.
- [x] Documentar `PartialEq` de floats, `NaN`, comparação por bits e tolerância
  como políticas distintas; exigir publicação reativa para revisão nova.

Signals disponíveis em `src/runtime/signals.rs` e `src/handle.rs`, com exemplos
em `src/lib.rs`. Leituras rastreadas capturam revisões provisórias por reader;
observers são marcados iterativamente, sem executar closures. A publicação/
reconciliação bidirecional das dependências está implementada na etapa 3; construtores
públicos de effects, filas/flush e hook de agendamento pertencem às etapas 4/6.
Set igual evita também alocar um snapshot novo. Valores/comparadores retirados
são liberados individualmente fora dos borrows, após marcar dependentes.

## 3. Grafo, memos lazy e tracking dinâmico

Referências: seções 2, 3.1, 4, 5 e 9.

### Marcação descendente

- [x] Implementar observers e sources bidirecionais, com índices para remoção
  coerente dos dois lados de cada aresta.
- [x] Marcar observers diretos de signal alterado como `Dirty` e descendentes
  como `Check`, sem executar computações nessa fase.
- [x] Usar pilha explícita reutilizável, ignorando nós descartados.
- [x] Podar propagação quando o estado já cobre a marcação; propagar aos
  observers quando o nó estava `Clean`.
- [x] Atualizar revisões de freshness conforme a seção 4.4, sem scans globais.
- [x] Registrar invalidação durante `Executing` mesmo em nó já `Dirty`.
- [x] Integrar a marcação à pertença lógica à fila conforme a seção 6.1.1
  com as filas de effects da etapa 4.

### Memos e atualização ascendente

- [x] Implementar `memo(compute)` e `memo_with(compute, comparator)` puros, sem
  setter público, com sources, observers e valor inicialmente não inicializado.
- [x] Usar `FnMut(&Runtime) -> Result<T, RuntimeError>` e adaptadores infallible;
  permitir estado privado apenas para derivação/cache puro.
- [x] Executar primeira avaliação na primeira leitura, nunca retornar valor fictício.
- [x] Atualizar memo necessário tanto em `try_get` quanto em `try_peek`.
- [x] Implementar fast path de memo inicializado `Clean`: validar handle/tipo/
  poison e fase, obter snapshot e registrar leitura, sem verificar sources,
  abrir contexto de avaliação ou tocar filas.
- [x] Implementar `update_if_necessary` com verificação de `Check` na ordem da
  última primeira leitura, sem tracking da verificação interna.
- [x] Comparar revisão do valor da source com a revisão consumida na aresta,
  inclusive quando a source já está `Clean`.
- [x] Interromper verificação na primeira source que confirma `Dirty`; limpar
  `Check` sem executar quando nenhuma source mudou de valor.
- [x] Percorrer sources por cursor/ID e borrows curtos, sem copiar a lista;
  revalidar reader após atualizações que executem código de usuário.
- [x] Implementar protocolo `Cleaning` → `Executing`, preservando contexto
  externo e executando a closure fora do armazenamento emprestado.
- [x] Revalidar nó/runtime após `Cleaning` antes de executar a closure.
- [x] Preparar reconciliação sem publicá-la antes de sucesso de compute e
  comparador; comparar memo fora de tracking e borrows internos.
- [x] Fazer commit de arestas/valor sem callbacks e revalidar reader antes dele.
- [x] Implementar equality cut: conservar snapshot anterior quando igual,
  descartar candidato fora do borrow e reconciliar dependências mesmo assim.
- [x] Ao mudar valor do memo, tornar observers `Check` em `Dirty` sem propagar
  desnecessariamente um subgrafo já marcado.
- [x] Preservar tickets de effects quando memo é atualizado lazy antes do flush.
- [x] Reutilizar valor do memo sem invalidação nova; permitir reexecução na
  mesma rodada se writes imediatos exigirem leitura fresca.
- [x] Nunca publicar memo com sources mortas/desatualizadas ou resultado falho;
  tratar divergência inesperada em memo puro como falha, não retry ilimitado.

### Tracking, deduplicação e reconciliação

- [x] Implementar pilha de `TrackingContext` com reader, epoch, cursor do
  prefixo estável, divergência, sources novas, deduplicação e revisões lidas.
- [x] Suspender/restaurar contexto externo em avaliações aninhadas; registrar
  somente o memo lido no reader externo, não suas sources internas.
- [x] Preservar ordem da primeira leitura e remover dependências não mais lidas.
- [x] Deduplicar por contexto, sem marcador único por fonte que perca informação
  de readers suspensos.
- [x] Implementar prefixo estável sem reescrita de arestas; estabelecer primeira
  leitura antes de avançar cursor, inclusive em sequência `A, A, B`, por
  correspondência à próxima source única ou fallback de deduplicação.
- [x] Ao divergir, reconstruir apenas sufixo deduplicado, mantendo ordem.
- [x] Começar deduplicação com buffer pequeno/busca linear e promover para
  conjunto reutilizável acima de limiar posteriormente medido.
- [x] Manter buffers suspensos exclusivos dos respectivos readers aninhados.
- [x] Capturar freshness e revisão do valor depois da atualização lazy, também
  no prefixo estável; preservar revisões da primeira leitura nas releituras.
- [x] Atualizar back-index da aresta movida ao usar `swap_remove` em observers;
  não usar remoção que desordene sources.
- [x] Reconciliar/validar revisões no mesmo commit interno, sem segunda cópia completa
  de sources, log de transação ou arestas provisórias por leitura.
- [x] Usar contadores largos com overflow explicitamente verificado em qualquer
  perfil; nunca wrap ou reset de epoch por scan global.
- [x] Avançar freshness de memo nas transições/execuções/publicações previstas
  na seção 4.4; não avançar por marcação redundante fora de execução.

Implementação em `src/runtime/graph.rs` e `src/runtime/memos.rs`, com leitura
pública em `src/handle.rs` e `Runtime::untrack`. O commit valida primeiro os
IDs/revisões capturados, no mesmo borrow interno, antes de alterar arestas;
não copia sources nem publica arestas provisórias. O prefixo mantém os índices,
e apenas o sufixo divergente é reconstruído. Buffers/conjunto são reutilizáveis;
o limiar inicial de 16 sources não é um resultado de benchmark.

Memos públicos são puros: não criam filhos nem registram cleanups. A transição
`Cleaning` descarta o conteúdo anterior e revalida o nó, com suporte de ownership
da etapa 5. A atualização lazy não altera tickets existentes
de effects; criação/enfileiramento/flush estão implementados na etapa 4.
Verificação ascendente é recursiva com limite de 512; marcação e
teardown são iterativos. Testes incluem comparação com DAG dinâmico de referência,
diamantes, prefixo/deduplicação, disposal cruzado, poison, ciclos e overflow.

## 4. Effects, filas, rodadas e flush

Referências: seções 2.2, 3.3, 4.4 e 6.1–6.4.

- [x] Implementar `effect(callback)` com
  `FnMut(&Runtime) -> Result<(), RuntimeError>` e adaptador infallible.
- [x] Criar effects `Dirty`, sem valor público/observers, agendados para primeira
  rodada, sem executar no construtor.
- [x] Implementar dois buffers reutilizáveis e pertença `None`, `Pending`,
  `Current(round)` independente da validade.
- [x] Garantir um ticket lógico por effect; tickets físicos carregam somente
  IDs geracionais, sem reter closures/valores por `Rc`.
- [x] Implementar todas as transições da tabela 6.1.1: absorção antes da execução,
  decisão adiada durante execução e próxima rodada após processamento.
- [x] Abrir rodada trocando buffers, filtrando tickets obsoletos e promovendo
  vivos a `Current(round)`.
- [x] Ordenar uma vez com `sort_unstable_by_key` por
  `(owner_depth, creation_sequence)`, sem confundir depth com ordem topológica.
- [x] Revalidar geração/vida de cada effect antes de processá-lo.
- [x] Garantir no máximo uma callback de effect por rodada; distinguir última
  rodada processada da última rodada com execução da callback.
- [x] Consumir ticket de effect `Check` limpo por equality cut sem contabilizar
  execução de callback.
- [x] Encaminhar effects criados na rodada e reinvalidados após processamento
  somente à próxima rodada, sem alterar ilimitadamente o snapshot atual.
- [x] Permitir writes imediatos em effects com marcação normal.
- [x] Consumir mudanças de `Checking`/`Cleaning` antes da closure, sem rodada extra.
- [x] Implementar conclusão por vida/revisão das sources realmente lidas:
  `Clean` se frescas; `Dirty` por mudança confirmada/disposal; ao menos `Check`
  por mudança potencial de memo.
- [x] Reenfileirar uma vez quando conclusão exigir próxima rodada; usar revisão
  consumida por aresta para não perder mudanças anteriores à publicação da aresta.
- [x] Não reenfileirar apenas por alteração de source antiga abandonada ou por
  writes de effect sem sources.
- [x] Não atualizar memos extras na conclusão só para confirmar revisão.
- [x] Implementar contadores de tickets vivos por buffer e
  `has_pending_effects()` em `O(1)`, inclusive após disposal.
- [x] Compactar tickets mortos por limiar mínimo/fração, amortizando retenção
  mesmo em ciclos de criação/disposal sem flush.
- [x] Nunca reorganizar `Current` com cursor ativo; durante callbacks compactar
  somente `Pending`, cancelando tickets atuais via metadata/contadores.
- [x] Drenar/compactar buffers em pontos seguros e liberar buffers vazios grandes
  conforme política explícita inicial; calibrar por medição na etapa 9.
- [x] Implementar `flush(options)` com guard contra flush reentrante.
- [x] Implementar `FlushOptions` com limite de rodadas e budget de trabalho/
  deadline cooperativo entre rodadas, nunca interrupção de closure.
- [x] Evitar consulta de relógio por read/aresta; documentar que rodada iniciada
  termina e pode ultrapassar budget.
- [x] Implementar `FlushReport` com rodadas, callbacks executadas, trabalho
  restante, falha e resultados `Stabilized`, `Yielded`, `RoundLimit`.
- [x] Preservar grafo e buffer `Pending` ao parar; não limpar nós para encerrar
  nem envenenar runtime por yield/limite de rodadas.
- [x] Distinguir loop provável de effects de ciclo real de dependências.

Implementado em `src/runtime/effects.rs`, integrado à marcação/disposal do grafo.
`effect_infallible` adapta callbacks sem `Result`. `FlushOptions` expõe
`max_rounds` (100 por padrão), `max_work` (tickets vivos processados, inclusive
equality cuts) e `deadline: Option<Instant>`. Budgets são checados apenas entre
rodadas; uma rodada iniciada termina, exceto por falha, e pode exceder o budget.
O relatório distingue `Stabilized`, `Yielded`, `RoundLimit` e `Failed`, contando
também callbacks que retornaram erro. Reentrada/poison prévio/closed são erros
de entrada; falha de avaliação retorna relatório e poison; panic propaga após
restaurar contextos e transferir tickets atuais restantes para `Pending`.

Contadores permitem consulta O(1), e tickets guardam apenas IDs geracionais.
Compactação inicial usa ao menos 64 mortos e 50% do buffer; buffers vazios com
capacidade acima de 4096 são liberados em pontos seguros. Esses limiares são
conservadores, não resultados de benchmark, e aguardam calibração na etapa 9.
Durante callbacks apenas `Pending` é compactado. Execução usa seu owner de conteúdo,
separado do owner de vida, com renovação automática/cleanups/filhos na etapa 5.
Testes cobrem as transições de fila, writes/revisões, equality cut, limites,
cancelamento/reuso de slots, compactação, reentrada, erros, panics e ciclos.

## 5. Ownership completo, disposal, pureza e falhas

Referências: seções 6.5, 7, 8 e 9.1.

### Owners e cleanups

- [x] Separar owner da vida da computação do owner do conteúdo de sua execução.
- [x] Descartar filhos/cleanups anteriores antes da reexecução, preservando o nó.
- [x] Reutilizar escopo vazio de execução sem alocar owner em toda rodada;
  renovar geração de handles públicos de conteúdo sem ressuscitar filhos.
- [x] Implementar `on_cleanup(callback)` no owner vivo e com argumento runtime.
- [x] Implementar `dispose(owner_or_effect)` sem executar effects pendentes.
- [x] Marcar todo subtree `Disposing` antes de callbacks, por travessia explícita
  do subtree, rejeitando novas inscrições e leituras de fontes removidas.
- [x] Desvincular arestas/nós/tickets lógicos antes de cleanups/destrutores;
  descartar descendentes antes do pai e cleanups em ordem reversa ao registro.
- [x] Invalidar gerações e liberar slots/closures/valores fora de borrows internos.
- [x] Invalidar dependentes sobreviventes de fontes descartadas como `Dirty`,
  removendo arestas sem permitir cache antigo confiável.
- [x] Implementar disposal idempotente de handle antigo do mesmo runtime e
  no-op para alvo já `Disposing`, sem tocar slot reutilizado.
- [x] Permitir self-disposal/owner-disposal em effects sem interromper callback;
  rejeitar inscrição no owner morto e não recorrer a owner implícito.
- [x] Após callback ou cleanup, revalidar reader antes de restaurar closure,
  publicar arestas/valor ou reenfileirar; abandonar commit se descartado.
- [x] Não reciclar contexto/buffer ativo até a callback retornar.
- [x] Tratar source nova lida e descartada sem publicar aresta morta;
  reenfileirar sobrevivente conforme protocolo de conclusão.
- [x] Executar cleanups sem reader implícito; permitir writes em signals vivos
  de runtime saudável e disposal de outros alvos, mas nenhuma criação ou flush.
- [x] Implementar `shutdown()` bloqueando inscrições e descartando todos os roots,
  inclusive após poison, sem effects ou reativação.
- [x] Documentar teardown explícito antes de liberar última fachada; liberação
  final do armazenamento não substitui cleanups por callbacks em `Drop`.

### Pureza, capacidades e segurança de borrows

- [x] Implementar toda a matriz de permissões da seção 6.5 para host, memo,
  effect, `Updating`, comparador, cleanup, hook e destrutor.
- [x] Tornar capacidades cumulativas por thread, inclusive entre runtimes;
  `untrack`, batch e owner aninhado não ampliam permissões.
- [x] Manter verificação de capacidade compacta e caminhos de erro frios, sem
  usar TLS para resolver handles/runtime implícito.
- [x] Rejeitar writes/criação/disposal/registro de cleanup em memo puro.
- [x] Rejeitar todo acesso reativo por comparador, hook ou destrutor executado
  pelo runtime, inclusive flush síncrono sem flush externo ativo.
- [x] Rejeitar read rastreado de outro runtime; permitir acesso explícito sob
  untrack sem dependência/invalidação cruzada.
- [x] Garantir que nenhuma callback de usuário roda com borrow interno ou
  referências a elementos de vetores incompatíveis com reentrada permitida.
- [x] Aplicar fronteira fora de borrows a compute, comparadores, `T::clone()`,
  update, hooks, cleanups, valores e closures removidas.
- [x] Não liberar incidentalmente último `Rc` ou recursos de usuário sob borrow;
  separar buffers de IDs/revisões dos buffers com valores de usuário.
- [x] Documentar pureza externa não interceptável e exigir `Drop` de snapshots
  sem efeitos reativos, inclusive quando último snapshot é liberado pelo usuário.
- [x] Manter implementação inicial segura, sem raw pointers, `UnsafeCell` ou
  `unsafe impl Send/Sync` como remédio para conflitos de borrow.

### Ciclos, poison e unwind

- [x] Detectar ciclo em qualquer fase ativa, tanto em verificação de `Check`
  quanto na execução. Caminho enriquecido condicionado à instrumentação da etapa 7.
- [x] Implementar guard de profundidade com erro distinto de ciclo para DAGs longos.
- [x] Manter erros locais sem mutação da operação rejeitada e sem poison;
  tornar erro propagado pela closure uma falha de callback.
- [x] Envenenar em falha de avaliação, ciclo/profundidade, callback falha,
  overflow de revisão/rodada/sequência ou violação de invariantes.
- [x] Preservar primeira falha e impedir recuperação granular/reset implícito,
  mesmo quando uma callback externa captura erro de avaliação.
- [x] Pós-poison, rejeitar reads/writes/criação/flush, bloquear hooks/effects e
  permitir somente diagnóstico, disposal e shutdown.
- [x] Preservar validade de snapshots entregues, sem prometê-los atualizados.
- [x] Em `Err`/panic de compute/effect, abandonar tracking provisório, preservar
  arestas anteriores vivas e manter nó inválido.
- [x] Devolver closure retirada somente ao slot original ainda vivo para
  disposal futuro; liberar por teardown caso nó tenha sido descartado.
- [x] Em falha de comparador, não publicar candidato nem confiar no valor anterior.
- [x] Com unwind, capturar panics de código de usuário, restaurar/desvincular
  recursos e só depois propagar primeiro panic com `resume_unwind`.
- [x] Documentar invariantes de eventual `AssertUnwindSafe`, sem alegar
  recuperação de closure por ter capturado panic.
- [x] Ao falhar cleanup, continuar teardown e tentar restantes uma única vez,
  em ordem reversa, capturando falhas individualmente e rejeitando writes
  pós-poison; preservar primeira falha e registrar secundárias.
- [x] Fazer guards `Drop` restaurarem apenas metadata/transferirem recursos,
  sem callbacks durante unwind; drenar recursos na fronteira externa.
- [x] Tratar payloads de panic e destrutores secundários sem novo panic durante
  unwind; documentar retenção excepcional e limites de double panic do Rust.
- [x] Validar suíte de panic safety em binário efetivamente `panic=unwind`;
  documentar ausência de cleanup/restauração em `panic=abort`.

Ownership completo em `src/runtime/ownership.rs`. `on_cleanup` aceita
`FnMut(&Runtime) -> Result<(), RuntimeError>` tentada uma única vez, e
`on_cleanup_infallible` adapta callbacks sem erro. Manter a closure fora do
`catch_unwind` permite drenar seus captures após panic da callback, separadamente.
Teardown devolve owners/nós retirados para uma fronteira sem borrow; todos os
slots/arestas/tickets do subtree são desvinculados antes do primeiro cleanup.
Execuções têm owner de conteúdo próprio, criado na primeira avaliação e renovado
por geração; escopos vazios e vetores de conteúdo são reutilizados. Buffers de
tracking de readers descartados permanecem retidos até seu guard terminar.

Falhas de cleanup são fatais, mas não interrompem as tentativas restantes.
`first_failure()` não é substituída; `secondary_failures()` registra falhas
posteriores de teardown. `dispose`/`shutdown` devolvem o primeiro erro de cleanup
da operação ou propagam o primeiro panic depois da drenagem. Liberação final da
fachada descarta captures sem invocar callbacks de cleanup. Não há reset/retry
pós-poison. Invariantes de unwind, limites de double panic, responsabilidade de
`Drop` de snapshots e exemplo de cleanup estão documentados em `src/lib.rs`.

Hooks/batch estão integrados na etapa 6, usando a matriz de capacidades e fronteira
de recursos. Diagnóstico enriquecido de caminho
de ciclo depende da instrumentação opcional da etapa 7; a detecção fatal funciona
em todas as fases agora. Testes cobrem LIFO/descendentes, self-disposal em execução
e Cleaning, limpeza de filhos enfileirados, reuso de buffers/gerações, pureza
cumulativa, erros/panics secundários e abandono de tracking com arestas preservadas.

## 6. Batch, untrack e notificações ao host

Referências: seções 3.2, 3.3 e 6.5.

- [x] Implementar `batch(callback)` aninhado agrupando apenas notificações;
  manter writes/marcação imediatos e reads frescos, sem rollback/isolamento.
- [x] Implementar `untrack(callback)` suspendendo/restaurando reader externo,
  sem remover tracking próprio de memos avaliados dentro da callback.
- [x] Implementar `request_flush` hook desacoplado de janela/event loop/frame.
- [x] Implementar `notification_outstanding` separada de filas/validade e armar
  antes da callback para coalescer pedidos.
- [x] Notificar apenas com trabalho vivo e sem batch, avaliação, disposal ou
  flush ativos; chamar hook sempre fora dos borrows.
- [x] Consumir notificação na entrada do flush, suprimir hook durante flush e
  reagendar ao sair com trabalho restante após restaurar guards.
- [x] Notificar em saída segura de batch externo, avaliação lazy e disposal.
- [x] Implementar instalação/substituição de hook desarmando notificação antiga
  e verificando trabalho; remoção preserva filas para flush manual.
- [x] Implementar `rearm_flush_notification()` para host que cancelou callback.
- [x] Rejeitar mutação/flush síncrono pelo hook com `Reentrancy`.
- [x] Documentar responsabilidade do host por executar ou rearmar callback e
  distinguir effects pendentes de trabalho gráfico pendente.

Implementação em `src/runtime/scheduling.rs`, com fronteiras integradas a writes,
construtores, avaliação lazy, disposal e flush. `batch` recebe callback fallible
e devolve seu resultado sem rollback nem poison em `Err` local. `untrack` mantém
sua API genérica e restaura o reader mesmo em panic. Hooks são `FnMut(&Runtime)`
infallible instalados por `set_request_flush_hook`, removidos por
`clear_request_flush_hook`; `flush_notification_outstanding()` é diagnóstico.
O host deve apenas agendar seu callback/evento no hook, nunca executar effects ali.

A flag é armada antes da callback; pedidos coalescem até entrada válida de flush,
troca/remoção/rearm. Saída com trabalho restante solicita novo callback após os
guards, inclusive em yield/limite. `shutdown` desarma e libera o hook fora de
borrows. Remoção do hook também é teardown de recurso permitido após poison,
sem alterar filas nem permitir recuperação. Guards nunca notificam em `Drop`;
panic no hook envenena, restaura sua closure para teardown e só então propaga.
Resultados de operações permanecem fora da captura do hook para liberar seus
recursos com segurança se a notificação entrar em panic.

Não há registro global de runtimes: mutações em runtime estrangeiro feitas sob
capacidades restritas não executam hook síncrono ali. O host deve rearmar esse
runtime depois da callback, ou realizar outra operação notificadora segura.
Testes cobrem saída lazy segura nesse cenário, batches aninhados/Err/panic,
reads frescos, pureza cumulativa, coalescimento, troca/remoção/rearm, disposal,
reagendamento após limites, reentrada proibida e panic com destrutor secundário.

## 7. Diagnósticos e verificadores

Referências: seções 3.3, 8.1, 10, 11 e 14.

- [x] Implementar diagnósticos opcionais: nós, arestas, marcações, verificações,
  execuções, rodadas, tickets e tamanhos de filas.
- [x] Com diagnósticos habilitados, expor caminho enriquecido de ciclo detectado
  nas fases ativas, sem construir traces no caminho normal.
- [x] Separar `FlushReport` de snapshot opcional do grafo; não formatar logs,
  construir traces ou percorrer grafo no caminho normal.
- [x] Implementar verificador de arestas bidirecionais/back-indices e sources
  deduplicadas/ordenadas em builds de teste.
- [x] Implementar verificador de filas, pertença lógica, contadores de tickets
  vivos e limite de uma execução de effect por rodada.
- [x] Validar todos os 17 invariantes da seção 10 em testes/verificadores,
  incluindo freshness, poison cumulativo, disposal e equality cut.
- [x] Verificar integridade do grafo após falha/teardown, sem presumir que nós
  poisoned ainda tenham valores derivados confiáveis.
- [x] Documentar complexidades da seção 11 e medir operações sem trabalho
  global, distinguindo custo de callback/valor do overhead do runtime.
- [x] Distinguir validação de revisões de conclusão completa de effects;
  cancelamento lógico de compactação de filas; somar todas as rodadas de flush
  sem promessa rígida de budget; separar grafo lógico, owners/reads/filas e
  capacidades históricas (inclusive limpeza de HashSet); explicitar ascensão
  recursiva com limite 512 versus marcação/teardown iterativos.

Features independentes `diagnostics` e `verification`, ambas desabilitadas por
padrão. Instrumentação, pilha diagnóstica e conjunto de execução por rodada são
removidos por `cfg` nos builds normais; verificadores também compilam nos testes.
Contadores saturam sem interferir em revisões/poison. Snapshots não retêm valores
ou closures; IDs são apenas diagnósticos, escopados pela identidade do runtime.
Auditorias são explícitas, sem scan automático por write/read. Caminho de ciclo
é materializado apenas na falha, com fases Checking/Cleaning/Executing ativas.

APIs e metodologia em `docs/diagnostics.md`. A cobertura dos 17 invariantes combina
auditorias estruturais, detecção de execução duplicada por rodada e testes
comportamentais existentes (não se tenta provar freshness executando callbacks
no verificador). A suíte randomizada de DAG usa também a auditoria completa.
Testes de corrupção exercitam back-indices, duplicatas, gerações, ownership e
contadores/representação de tickets. Contadores demonstram ausência de visitas
a computações não relacionadas; medições temporais seguem o experimento Criterion,
sem afirmar custo constante global ou performance superior.

## 8. Suíte obrigatória de testes

Referência: seção 14. Estes itens devem ser executados junto às etapas correspondentes.

### Correção do grafo

- [x] Diamante consistente, com memo final executado uma vez sem writes novos.
- [x] Equality cut em signal e em múltiplas profundidades de memos.
- [x] Memo lazy atualizado por leitura antes do flush.
- [x] Dependência condicional adicionada/removida com resultado igual e diferente.
- [x] Reordenação de sources e short-circuit na primeira mudança confirmada.
- [x] Leituras duplicadas, inclusive memo aninhado entre leituras da mesma source.
- [x] Prefixo estável `A, A, B` sem duplicata ou divergência falsa.
- [x] `peek`/`untrack` de memo sujo com tracking interno e sem reader externo.
- [x] Back-indices após remoção e `swap_remove` de observers.
- [x] Nós não afetados não executados nem visitados na atualização.

### Rodadas e scheduling

- [x] Muitos writes antes do flush com único ticket de effect.
- [x] Effect escreve na própria source sem perder reinvalidação.
- [x] Effect lê source nova e a escreve antes de publicar aresta.
- [x] Write em dependência de effect já processado encaminhado à próxima rodada.
- [x] Write em effect ainda aguardando absorvido sem ticket extra/rodada vazia.
- [x] Mudança de memo durante `Checking` sem rodada extra desnecessária.
- [x] Write de cleanup antes da closure consumido pela leitura nova.
- [x] Source antiga abandonada e alterada sem reexecução por aresta abandonada.
- [x] Memo novo lido, source alterada e memo atualizado antes da conclusão:
  reexecução por valor consumido diferente e corte quando valor final igual.
- [x] Primeira leitura → write → releitura sem sobrescrever revisão capturada.
- [x] Memo lido, invalidado e relido na mesma rodada com valor fresco.
- [x] Effect criado durante rodada executado somente na próxima.
- [x] Batch aninhado com reads frescos e restauração após panic.
- [x] Hook coalescido, sem borrow interno e sem flush reentrante.
- [x] Flush pelo hook rejeitado mesmo sem flush externo ativo.
- [x] Cancelamento pelo host, rearm e instalação/troca/remoção de hook.
- [x] Yield/limite preservando trabalho e permitindo reagendamento.
- [x] Ordem determinística com mesmo depth, inclusive após reenfileiramento.
- [x] Contadores após disposal/compactação sem flush, sem pendência falsa.

### Vida, segurança e erros

- [x] Pai descarta filho ainda presente na fila.
- [x] Self-disposal/owner-disposal durante callback com slot reutilizado antes
  do retorno, sem restauração de closure ou commit no ocupante novo.
- [x] Cleanup descarta computação antes da closure, impedindo sua execução.
- [x] Source nova descartada após leitura sem aresta morta; sobrevivente pode
  abandonar handle na reexecução.
- [x] Disposal idempotente, cleanup exatamente uma tentativa e ordem correta.
- [x] Slot reutilizado rejeita handle antigo; runtime diferente é rejeitado.
- [x] Fonte descartada invalida dependente sobrevivente sem cache confiável antigo.
- [x] Ciclos direto/indireto e ciclo durante travessia de `Check`.
- [x] Loop de writes limitado por rodadas sem stack overflow.
- [x] Profundidade excessiva com erro definido ou caminho iterativo.
- [x] Writes em memo/comparador rejeitados.
- [x] Criação/disposal/cleanup em memo e acesso do comparador ao grafo rejeitados.
- [x] Untrack, batch, owner e runtime aninhados não removem pureza.
- [x] Read cross-runtime sob tracking rejeitado; untrack sem aresta cruzada.
- [x] Update com write reentrante/disposal/panic em `Clone` sem sobrescrita,
  retry automático ou borrow interno durante código do usuário.
- [x] Overflow simulado com contadores injetados perto do limite para identidade, geração,
  revisão, epoch, sequência e rodada, sem wrap e com aposentadoria correta.
- [x] Panic em compute/comparador/cleanup sem nó falsamente `Clean`.
- [x] Valores type-erased/closures descartados sem aliasing ou leaks lógicos.
- [x] Cleanup falha e restantes são tentados uma vez, preservando primeira
  falha, rejeitando writes pós-poison e concluindo teardown.
- [x] Panic em hook/destrutor com poison/unwind no binário apropriado.
- [x] Snapshot entregue sobrevive ao disposal; pós-poison APIs falham e
  diagnóstico/disposal/shutdown permanecem disponíveis.
- [x] Erro local tratado mantém runtime saudável; erro de avaliação capturado
  externamente não remove poison.

### Propriedades

- [x] Criar modelo simples de referência para resultados de reads e execução.
- [x] Gerar sequências aleatórias de reads/writes/branching/disposal e comparar
  com modelo, verificando invariantes de grafo/filas após operações.
- [x] Confirmar custo/retenção limitados em histórico de handles mortos,
  reutilização de slots e criação/disposal repetidos sem flush.

Consolidação e matriz rastreável em `docs/testing.md`. Novas regressões e modelo
em `src/runtime/conformance.rs` (somente `cfg(test)`), complementando os testes
das etapas anteriores, sem duplicar a suíte inteira ou adicionar dependências.
Modelo independente de publicações/observação executa 32 seeds × 1.000 operações
com 8 grafos dinâmicos, comparando valores e callbacks e auditando invariantes
depois de cada operação. Criação/disposal de 2.000 grafos sem flush confirma
reuso estrutural, cleanup único, drop dos recursos e rejeição de handles antigos.
Overflow é simulado por injeção nos contadores reais perto de `u64::MAX`, não por
redução da largura dos tipos em produção. Isso exercita os mesmos checked-adds
e aposentadoria sem exigir loops impraticáveis. Tempo/allocations permanecem
assuntos da etapa 9; testes estruturais não afirmam latência constante.

## 9. Benchmarks, memória e otimizações medidas

- [x] Investigar discrepância de chains com scaling, decomposição, perf real
  por crate, preflight de execuções e allocations/contadores separados. Sem
  modificar algoritmos do runtime: resultados/prioridades em
  `benches/.log/CHAIN-INVESTIGATION.md`. Candidatos futuros não estão autorizados
  somente por esta investigação.

- [x] Conclusão de effect: documentar custo completo O(k + m − p), fundir
  recuperação do prefixo/validação e evitar self-copies de reads sem remoções.
  Medir fan-in estável e contração de width + 1 sources para uma, sem esconder
  remoção de sufixo no timer. Resultados modestos e limites em
  `benches/.log/EFFECT-COMPLETE-RESULTS.md`.

- [x] Shutdown em duas fases para a floresta inteira: tornar todas as roots/
  owners/nós indisponíveis antes de remover arestas, sem invalidar readers que
  também morrerão. Preservar ordem inversa de roots/BFS, cleanups LIFO,
  primeira falha, panic drain e tracking ativo. Benchmark fan-in/fan-out/roots
  independentes em 64/256/1.024/4.096, mais allocations/contadores.
  Resultado (incluindo regressão): `benches/.log/SHUTDOWN-RESULTS.md`.

- [x] Eliminar compactação repetida após disposals individuais de sources:
  tombstones somente de IDs/revisões, posições/back-indices vivos estáveis,
  reconciliação única e descarte imediato de valores. Medir individual/bulk
  fan-in 64/256/1.024/4.096 incluindo reconciliação; validar slot reuse,
  equality cut, poison/shutdown e diagnóstico lógico/físico.
  Resultados e ressalvas em `benches/.log/DISPOSAL-RESULTS.md`.

Experimento inicial em `benches/comparison.rs` com Criterion, comparando
`reactive`, `sycamore_reactive` e `leptos_reactive`: signals sem subscribers,
chains, diamonds e fan-out, com preflight de valores e contagem de effects.
Metodologia, diferenças semânticas e comandos em `benches/README.md`.
Esse experimento foi estendido nesta etapa com dependências condicionais,
decomposição, memória, calibração e runs release preservados. Smoke tests
continuam sendo apenas validação, não baseline de performance.

Referências: seções 5, 9, 11, 14 e 15.

- [x] Estabelecer baseline seguro antes de mudar layout/algoritmo por performance.
- [x] Criar benchmarks de chains, diamonds, fan-out alto e dependências condicionais.
- [x] Medir marcação, verificação, execução, igualdade, tracking, reconciliação,
  ordenação/compactação de filas e disposal separadamente.
- [x] Contar callbacks, rodadas e tickets para identificar reexecuções/rodadas
  redundantes, não apenas tempo total.
- [x] Medir allocations e memória retida após aquecimento e após disposal.
- [x] Separar custo de clone de `Rc<T>`, clone de `T` em update e alocação do
  resultado de memo do custo do scheduler.
- [x] Medir baixo/alto fan-in e readers aninhados e selecionar limiar de promoção
  de deduplicação com base nos resultados.
- [x] Comparar experimentalmente AoS e hot/cold com os mesmos grafos em surrogate,
  considerando cache locality, memória e indireções.
- [x] Reutilizar pilhas, filas, buffers de tracking e prefixos estáveis para
  minimizar allocations recorrentes em grafos aquecidos.
- [x] Medir/restringir retenção de buffers grandes e escolher limiares de
  compactação/retenção sem custo linear por disposal individual.
- [x] Verificar `has_pending_effects()` em `O(1)`, conclusão por revisões em
  `O(k)`, ordenação em `O(E log E)` e espaço lógico em `O(V + A)`, explicitando
  retenção física high-water das arenas/capacidades.
- [x] Usar perfil `fast-dev` durante desenvolvimento; usar `release` somente
  para benchmarks comparáveis ao baseline release e informar política de panic.

Runs, metadados, distribuições e métricas em `benches/.log`; resumos versionáveis
e logs brutos ignorados por padrão. Runner/análise em `benches/run.py` e
`benches/analyze.py`; metodologia em `benches/README.md`; decisões e limitações
em `benches/.log/RESULTS.md`. Comparação: 45 casos; decomposição: 75; memória:
39 cenários, allocator somente no executável separado, diagnostics opt-in.
Profiling perf real com símbolos, sem temporizadores em produção. Janelas pela
API pública incluem componentes explicitados; amostragem por símbolo separa
hotspots, sem fingir timers puros de cada função interna.

Mantidos renew in-place de scope vazio (com fallback de aposentadoria), liberação
sem guard quando nenhum destructor pode rodar e limiar de dedup 128 após sweeps
16/32/64/128. Confirmado reuso de buffers sem reallocations após warmup; as
allocations restantes são snapshots/candidatos. Política de fila 64 mortos/50%
e retenção vazia até 4.096 slots foi medida e mantida, sem scan global por disposal.
Layout foi somente experimento surrogate de stride/localidade, não comparação
de duas implementações completas do runtime nem mudança de layout do núcleo.
As tarefas condicionais não fizeram parte da etapa 9 original; o experimento
SmallVec solicitado posteriormente está registrado abaixo.

Otimizações posteriores de tracking/checagem são medidas separadamente em
`benches/.log/PREFIX-RESULTS.md` e `benches/.log/CLEAN-RESULTS.md`. Sources
inicializadas Clean/Idle de valor agora são verificadas em uma janela curta de
borrow, sem `update_if_necessary`; demais sources mantêm o caminho completo,
sem borrow durante callbacks. Baselines, repetições e memória em `prefix-*`/
`clean-*`; igualdade observada acrescentou quatro cenários antes do baseline.
Harness atual: 91 casos de decomposição e 55 de memória. Sem novo layout,
fields ou `unsafe`; ciclos, poison, first mismatch e overflow continuam ativos.

Revisão posterior de boundaries: mapa, assembly e candidatos em
`benches/.log/BOUNDARY-RESULTS.md`. Mantidos catches/guards/counters em todas as
operações; somente a saída externa tenta notificar, e `set_phase` concentra a
checagem Read contígua após cleanup. Fusão de fase/captura e inline forçado foram
testados e descartados por regressões; nenhum cold split adotado. Métricas e
assembly em `boundary-*`; `benches/analyze-boundaries.py` deriva o resumo.
- [x] (Condicional) Avaliar buffers inline/SmallVec ou especializações de layout
  somente com benefício medido e preservação dos invariantes.
  Experimento posterior solicitado: feature opt-in `experimental-inline-reads`,
  duas reads inline, sem mudança de edges/algoritmo. Testes/invariantes preservados,
  porém sem ganho consistente e com Node maior; **não adotado por padrão**.
  Runs em `benches/.log/reports/inline-*.json`, análise em `inline-analysis.json`
  e decisão em `benches/.log/INLINE-RESULTS.md`. Outros layouts não implementados.
- [ ] (Condicional) Implementar máquina iterativa de atualização ascendente se
  profundidade/custo justificar, mantendo marcação e disposal iterativos.
- [ ] (Condicional) Se introduzir `unsafe`, documentar aliasing/lifetimes,
  demonstrar ganho, revisar segurança e executar validação apropriada com Miri.

## 10. Integração experimental e documentação do host

Referências: seções 1, 11, 12, 13 e 15.

- [ ] (Experimental, após validar núcleo) Criar adaptador GPUI externo ao núcleo,
  mantendo scheduler gráfico independente e sem dependência GPUI no crate.
- [ ] Demonstrar input/mensagens → handlers/signals → hook → flush → effects
  atualizando propriedades retidas, sem reconstrução integral da interface.
- [ ] Documentar que host combina effects, layout/paint/hit-test dirty, animações,
  deadlines, resize/expose, superfície e trabalho/retries/conclusão GPU.
- [ ] Demonstrar wake-up do host por trabalho externo/conclusão GPU, mesmo sem
  movimento de mouse ou effects pendentes.
- [ ] Demonstrar avaliação lazy fora de frame/flush e ausência de associação
  implícita entre rodada reativa e frame gráfico.
- [ ] Documentar effects como observação de estado atual, não entrega de eventos;
  exigir fila externa para eventos/amostras que precisam de entrega individual.
- [ ] Demonstrar que coalescer flush não elimina amostras de desenho/input:
  acumular todas no integrador e publicar snapshots/revisões.
- [ ] Separar vida de componente, validade de cache e recursos GPU in-flight;
  atribuir liberação física a fences/retirement queues do backend.
- [ ] Documentar que fine-grained de cena/pintura exige identidades estáveis,
  revisões locais e invalidação em outros crates; não usar redraw contínuo
  para ocultar falhas do runtime.
- [ ] Documentar batching sem transação/rollback/isolamento e glitch-free sem
  isolamento entre effects que escrevem sequencialmente.
- [ ] Auditar limites do núcleo: nenhuma dependência de GPUI/Wayland/Vulkan/
  wgpu/Vello/Taffy, janela, event loop, layout, renderer ou gestão de VRAM.
- [ ] Auditar ausência de conceitos de documento/stroke/câmera/ferramenta/widget,
  serialização do grafo como documento, simplificação geométrica ou ajuste de AA.
- [ ] Auditar ausência de broadcast global, diff de árvore inteira, scans de
  grafo por write/flush, owners imortais e callbacks reativos após disposal.
- [ ] Documentar limites de custo/alocação e de execução por rodada; não prometer
  trabalho constante, zero allocation absoluto ou uma execução universal por frame.
- [ ] Documentar transferência entre threads por mensagens externas, nunca
  acesso concorrente aos handles do runtime inicial.

## 11. Ergonomia opcional e extensões futuras

Referências: seções 3.1, 8.2 e 9.1. Não fazem parte dos contratos obrigatórios
da primeira versão; não alterar sua semântica silenciosamente.

- [ ] (Opcional) Oferecer `get()` infallible com falha explícita para acesso
  inválido, preservando a API fallible básica.
- [ ] (Opcional) Oferecer conveniência de leitura clonada com `T::clone()` fora
  dos borrows, mantendo `Rc<T>` como leitura normal.
- [ ] (Futuro) Avaliar API `with` somente após revisão de borrow/reentrância,
  sem expor `Ref` interno como contrato.
- [ ] (Futuro) Avaliar fachada contextual TLS sem substituir runtime explícito,
  esconder pertencimento ou ampliar capacidades.
- [ ] (Futuro) Especificar recuperação granular de computações antes de
  implementá-la; `catch_unwind` sozinho não concede recuperação.

## 12. Critérios de conclusão do núcleo

Referências: seções 10, 13, 14 e 15.

- [ ] Todas as APIs obrigatórias e contratos da primeira versão implementados.
- [ ] Todos os 17 invariantes cobertos por testes/verificadores.
- [ ] Suíte obrigatória passando sem janela, GUI ou GPU.
- [ ] Nenhum gap conhecido em reinvalidação, tracking aninhado, equality cut,
  disposal, overflow, scheduling ou protocolo de falha.
- [ ] Nenhuma callback de usuário executada sob borrow interno incompatível.
- [ ] Baseline de correção/performance e memória registrado antes de otimizações.
- [ ] Dependências e limites de escopo auditados; integração gráfica não é
  necessária para estabilizar ou validar o grafo.
- [ ] Documentação pública explica ownership, snapshots, pureza, reentrada,
  flush/budgets, poison, panic safety e responsabilidades do host.
