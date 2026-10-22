# Runtime de reatividade fine-grained

## 1. Objetivo e escopo

Construir um crate Rust independente para reatividade fine-grained, inspirado
em SolidJS e na abordagem de duas passagens do Reactively. Este documento é
uma especificação proposta, não uma descrição de código já implementado.

O runtime deve descobrir dependências por leitura, propagar invalidação e
executar somente computações necessárias. Deve permitir que uma futura GUI
atualize propriedades de nós retidos sem reconstruir toda a interface.

O runtime **não é** um store de documentos, framework de GUI, engine de layout,
renderer ou event loop. Pode ser utilizado com GPUI inicialmente e com outro
host no futuro, sem dependências dessas tecnologias.

Princípios:

- duas fases: marcação descendente e atualização ascendente;
- memos puros, preguiçosos e com corte por igualdade;
- effects explicitamente agendados, nunca executados dentro de `set`;
- dependências dinâmicas e deduplicadas por execução;
- ownership e descarte determinísticos;
- IDs geracionais, sem reutilização silenciosa de handles obsoletos;
- implementação segura e testável antes de otimizações com `unsafe`;
- nenhuma associação implícita entre uma rodada reativa e um frame gráfico.

## 2. O que o crate deve prever

### 2.1 Três tipos de nó

| Tipo | Valor | Sources | Observers | Estado de validade |
| --- | --- | --- | --- | --- |
| Signal | Escrito externamente | Não | Sim | Sempre `Clean` |
| Memo | Derivado por função pura | Sim | Sim | `Clean`, `Check`, `Dirty` |
| Effect | Sem valor público | Sim | Não | `Clean`, `Check`, `Dirty` |

```rust
#[repr(u8)]
enum State {
    Clean = 0,
    Check = 1,
    Dirty = 2,
}
```

- `Clean`: valor confiável relativamente às sources observadas.
- `Check`: há mudança potencial a montante; verificar sources antes de executar.
- `Dirty`: precisa executar, por invalidação direta ou primeira avaliação.

Signals armazenam imediatamente o novo valor e continuam `Clean`. É a validade
das computações dependentes que muda.

### 2.2 Validade não é agendamento

Não usar apenas `State` para controlar execução e filas. Prever, separadamente:

- fase de avaliação: `Idle`, `Checking`, `Cleaning` ou `Executing`;
- pertença lógica à fila: `None`, `Pending` ou `Current(round)`;
- revisão de freshness das fontes e invalidação durante execução;
- rodada da última execução de um effect;
- fila atual e fila da próxima rodada;
- inicialização do valor e estado de falha, quando aplicável.

Um memo não inicializado não pode retornar um valor fictício. Primeira leitura
executa a computação. Um effect recém-criado é `Dirty` e entra na fila; não
executa imediatamente no construtor.

Qualquer fase diferente de `Idle` mantém o guard de avaliação ativo para
detectar ciclos. `Checking` atualiza sources; `Cleaning` descarta o conteúdo
anterior; `Executing` executa a closure. Essas fases não são estados de validade.
Uma mudança confirmada durante `Checking` é consumida pela execução que vem
em seguida, não é, por si só, motivo para outra rodada.

### 2.3 Handles e pertencimento ao runtime

Handles tipados devem carregar identidade de runtime e ID geracional:

```text
Signal<T> / Memo<T> / EffectHandle / OwnerHandle
    runtime identity + slot index + generation
```

A identidade evita que handles de dois runtimes com índices iguais se confundam.
O runtime deve verificar o tipo do valor em qualquer acesso type-erased interno.

- Slots reutilizados recebem nova geração.
- Se a geração esgotar, aposentar o slot; não dar wrap silencioso.
- Handles obsoletos retornam erro nas APIs `try_*`.
- Entradas obsoletas em filas são ignoradas.
- A checagem defensiva de geração não substitui a remoção das arestas no disposal.
- Clonar/copiar um handle não estende a vida do nó; o owner controla sua vida.

A implementação usa SlotId de 8 bytes (índice u32 + geração NonZeroU32) e
HandleId de 16 bytes (RuntimeId u64 + SlotId). Índice u32::MAX é reservado;
esgotamento de índice retorna CounterExhausted antes de consumir recursos.
Geração u32 esgotada aposenta o slot, sem wrap. Os extratores da arena usam
unsafe somente após bounds/geração/vida para eliminar o teste redundante de
Option. Prova e limites em [handle-architecture.md](./handle-architecture.md).

A ergonomia pode oferecer handles `Copy`, sem exigir `T: Copy`. Handles não
devem ser `Send`/`Sync` na implementação single-thread inicial. Transferência de
mensagens entre threads é responsabilidade do integrador, não acesso concorrente
ao grafo.

A API inicial adota handles pequenos `Copy` e acesso explícito ao runtime:
`signal.try_get(&runtime)`, `signal.try_set(&runtime, value)` e equivalentes.
Implementar `Copy`/`Clone` manualmente quando necessário para não impor bounds
sobre `T`; um marcador como `PhantomData<Rc<T>>` impede `Send`/`Sync` sem guardar
um `Rc` por handle. Os handles não contêm ponteiros dereferenciáveis nem procuram
o runtime em um registro global. A identidade do runtime é única no processo,
não reutilizada; esgotamento de seu contador é erro, nunca wrap.

`Runtime` é uma fachada clonável sobre `Rc<RuntimeInner>`. Clonar essa fachada
mantém o armazenamento vivo, não substitui disposal dos owners. Closures recebem
`&Runtime` como argumento: não precisam capturar um `Runtime` forte, evitando o
ciclo `runtime → closure → runtime`. Captura forte permanece possível em Rust,
mas deve ser documentada como ciclo de retenção; usar handles ou uma fachada
fraca para referências externas de longa duração. Não há acesso implícito via
TLS na API inicial.

## 3. API pública proposta

Os nomes abaixo descrevem contratos; assinaturas finais dependem do desenho de
lifetimes, armazenamento de valores e ergonomia de closures.

Valores e closures armazenados são `'static`. Leituras distinguem cópia de
`T: Copy`, clone explícito de `T: Clone` e empréstimo temporário de `&T`.
Não existe snapshot `Rc<T>` implícito: compartilhamento exige armazenar `Rc<T>`
explicitamente como valor e usar uma leitura clonada.
Comparadores recebem `&T` e computações recebem `&Runtime`. Memos usam
`FnMut(&Runtime) -> Result<T, RuntimeError>` e effects usam
`FnMut(&Runtime) -> Result<(), RuntimeError>`; adaptadores infallible apenas
envolvem o resultado em `Ok`. `FnMut` não autoriza efeitos observáveis em memos:
estado privado só pode servir à derivação/cache puro.

### 3.1 Construção e leitura

| API | Contrato |
| --- | --- |
| `Runtime::new()` | Cria runtime independente, sem janela ou scheduler obrigatório. |
| `create_root(...)` / `create_owner(...)` | Cria escopo explícito de vida dos nós. |
| `signal(initial)` | Signal com igualdade padrão para valores apropriados. |
| `signal_with(initial, comparator)` | Política explícita de igualdade. |
| `memo(compute)` / `memo_with(compute, comparator)` | Valor derivado puro, avaliado quando necessário. |
| `effect(callback)` | Effect agendado, com handle para disposal. |
| `try_get_copy(&rt)` | Leitura rastreada por cópia, exige `T: Copy`. |
| `try_get_cloned(&rt)` | Leitura rastreada com `T::clone()`, exige `T: Clone`. |
| `try_with(&rt, callback)` | Leitura rastreada temporária com `&T`, sem Clone. |
| `try_peek_copy` / `try_peek_cloned` / `try_peek_with` | Mesmas formas sem tracking externo; atualizam memo quando necessário. |
| `untrack(callback)` | Suspende tracking no escopo da callback e restaura ao sair. |

Todas as leituras são fallible. `with` retorna o resultado da callback, mas a
referência emprestada não pode escapar. Callback emprestada e `T::clone()`
bloqueiam operações reativas, inclusive reads e operações em outro runtime.
Não há borrow do grafo durante user code; somente a célula do valor está
emprestada. Panics restauram borrows/capabilities e envenenam o runtime.

### 3.2 Escrita e escopos

| API | Contrato |
| --- | --- |
| `try_set(value)` | Compara, escreve imediatamente, marca dependentes e solicita agendamento. |
| `try_update_cloned(&rt, callback)` | Para `T: Clone`, edita uma cópia fora do grafo; valida conflito e aplica `set`. |
| `batch(callback)` | Agrupa pedidos de agendamento; escritas e marcação continuam imediatas. |
| `on_cleanup(callback)` | Registra cleanup no owner atual. |
| `dispose(owner_or_effect)` | Descarta escopo/nó sem executar effects pendentes. |

Convenções:

- não exportar setter para memos;
- callbacks de `update` não recebem referência mutável ao valor dentro do grafo;
- reads dentro de `batch` continuam vendo valores frescos;
- `batch` não é transação, rollback ou isolamento;
- scopes aninhados restauram contexto mesmo em saída antecipada ou unwind;
- ausência de owner na criação de nós deve ser um erro explícito, não um owner
  global imortal oculto.

`try_update_cloned` captura a célula e revisão do signal, executa `T::clone()`
sob restrições de leitura emprestada, libera o borrow do valor e executa a
callback sob `Updating`. Revalida o handle e a revisão antes de comparar ou
publicar. Se o signal foi alterado nesse intervalo, retorna `UpdateConflict`
sem sobrescrever a alteração; se foi descartado, retorna `InvalidHandle`.
A callback não é repetida automaticamente e seus efeitos externos não são
revertidos. Set de valor igual não muda a revisão. Para valores grandes,
preferir construir um valor novo e usar `try_set`, sem clonar toda a coleção.
Clone não permite reentrada; a callback de edição permite as operações de
`Updating` descritas na seção 6.5. Nenhuma API empresta `&mut T` do valor vivo.

`untrack` remove o reader externo somente durante a callback. Um memo atualizado
dentro dela ainda instala seu próprio reader e rastreia suas próprias sources.
Ao retornar, o reader suspenso é restaurado; a leitura externa feita via
`peek`/`untrack` não cria aresta. Pureza e restrições de reentrada não mudam.

### 3.3 Scheduler e observabilidade

| API | Contrato |
| --- | --- |
| `has_pending_effects()` | Informa se há effects para processar; não informa trabalho gráfico. |
| `flush(options)` | Executa rodadas até estabilizar, falhar ou alcançar limite. |
| `request_flush` hook | Notifica o host para agendar trabalho; não executa effects. |
| `FlushReport` | Rodadas, effects executados, trabalho restante e erro/limite alcançado. |
| `RuntimeError` | Handle inválido, runtime errado, ciclo, escrita proibida, reentrância, falha etc. |
| Diagnósticos opcionais | Contadores de nós, arestas, verificações, execuções e filas. |

O hook deve ser chamado fora de qualquer borrow interno do runtime e não pode
executar um flush síncrono/reentrante. Coalescer notificações enquanto o host já
tem trabalho agendado. Se `flush` parar com trabalho restante, permitir que o
host reagende: não perder a notificação por causa de uma flag antiga.

Usar uma flag `notification_outstanding`, separada de filas e validade. Uma
operação externa solicita o hook somente quando há trabalho vivo, não há batch,
avaliação, disposal ou flush em andamento e a flag está desarmada. Armar a flag
antes da callback. Ao entrar em `flush`, consumir essa notificação; writes
durante o flush não chamam o hook. Ao sair, restaurar os guards antes de
notificar novamente caso reste trabalho. A saída do batch mais externo e de
uma avaliação lazy/disposal também é um ponto seguro de notificação.

Instalar/substituir um hook desarma a notificação anterior e verifica trabalho
pendente. Removê-lo mantém as filas, permitindo flush manual. O host é
responsável por não descartar a callback agendada; se cancelá-la, deve chamar
uma operação explícita `rearm_flush_notification()` para permitir novo pedido.
O hook não recebe autoridade para mutar o grafo nem chamar `flush`, mesmo se
nenhum flush estiver ativo. Essas tentativas retornam `Reentrancy`.

É útil separar um relatório de flush de um snapshot opcional do grafo. O caminho
normal não deve formatar logs, construir traces nem percorrer todo o grafo.

## 4. Modelo de execução

### 4.1 Fase 1: marcação descendente

Em `set`, se o comparador considerar o valor igual, encerrar sem invalidar.
Caso contrário, substituir o valor e marcar observers diretos como `Dirty`.
Descendentes dessas computações recebem `Check`.

A propagação usa uma pilha explícita reutilizável:

```text
mark(root, requested_state):
    push(root, requested_state)
    while stack has item:
        resolve handle; skip if disposed
        record freshness change according to source revision rules
        record invalidation if Executing; Checking/Cleaning consume it before run
        if state already covers requested_state:
            ensure logical queue membership; defer Executing reschedule decision
            continue
        was_clean = state == Clean
        state = max(state, requested_state)
        ensure invalidated effect is queued exactly once
        if was_clean:
            push observers with Check
```

O corte de propagação evita revisitar subgrafos já marcados. A exceção importante
é a invalidação durante execução: ela precisa ser registrada mesmo quando o
estado já é `Dirty`.

Esta fase não executa memos nem effects. Entretanto, um comparador customizado
ou `PartialEq` é código do usuário: deve ser puro, sem tracking ou escritas no
grafo. Não afirmar que `set` nunca executa código do usuário em sentido absoluto.
Destrutores dos valores também exigem cuidado: liberar valores retirados fora
dos borrows internos e impedir reentrância indevida.

Em `set`, capturar a célula do valor, executar a comparação sob guard de pureza
fora do borrow, revalidar o slot e só então publicar e marcar. Nenhum destrutor
é chamado entre a publicação do novo valor e a conclusão da marcação. Valores
retirados são liberados depois, sob a política de destrutores da seção 6.5.
Comparadores não podem alterar o grafo, portanto não se exige retry da
comparação nem locking otimista adicional no caminho normal de `set`.

### 4.2 Fase 2: atualização ascendente

Uma leitura de memo ou processamento de effect chama `update_if_necessary`:

```text
update_if_necessary(node):
    verify handle and failure status
    if signal: return
    if node phase != Idle: report cycle
    if node is Clean and initialized: return
    enter evaluation guard
    if Check:
        phase = Checking
        visit existing sources in last-read order
        if source is a valid initialized value node, Clean and Idle:
            use its value revision directly, without user code
        else:
            release graph borrows; update source without tracking verification
            revalidate runtime, handles and revision afterward
        compare source value revision with the revision last consumed by node
        if different: mark node Dirty
        stop at first source that makes node Dirty
    if Dirty:
        execute computation with its own tracking context
    else if verification succeeded and no pending invalidation:
        mark Clean
    leave evaluation guard
```

Não limpar incondicionalmente o nó ao sair: uma execução pode ter falhado ou
recebido nova invalidação. O protocolo de conclusão é descrito abaixo.

Quando um memo é verificado e suas sources não produziram mudança de valor,
ele volta a `Clean` sem executar. Na reexecução, dependências que não forem mais
lidas deixam de existir.

O caminho de leitura de memo `Clean` valida handle/tipo/poison, verifica fase,
obtém a célula tipada e registra a leitura se houver reader; não abre contexto de
avaliação, não verifica sources e não toca filas. Só a atualização lazy entra
nas fases de avaliação. A verificação de fase precede o fast path para não
mascarar ciclo/reentrada com um snapshot anterior.

A checagem de sources já `Clean`/`Idle` pode fundir validação de handle, kind,
initialized, fase e revisão em uma janela curta de borrow. Somente Signal/Memo
são sources de valor; effect não é elegível. Runtime/capabilities devem estar
validados antes da janela. Sources com fase ativa ou não inicializadas seguem o
caminho completo, preservando ciclos/poison. Nenhum borrow atravessa callbacks
de compute/cleanup; após essas callbacks, revalidar antes de consumir a revisão.
O primeiro mismatch ainda encerra a visitação, e overflow checked de freshness
não pode ser ignorado por esse fast path.

A janela sem user code pode reutilizar o borrow entre sources e evitar uma
resolução mutável do reader quando a revisão coincide. Antes da ascensão
recursiva, liberar esse borrow; ao retornar, revalidar runtime, source e reader.
A ascensão continua recursiva e sujeita ao mesmo limite checked de profundidade.

### 4.3 Reexecução e corte por igualdade

Executar uma computação implica:

1. preservar o contexto externo de tracking/owner;
2. separar closure e célula necessária do armazenamento interno;
3. liberar borrows do runtime;
4. entrar em `Cleaning` e descartar filhos e cleanups do escopo anterior;
5. após os cleanups, iniciar `Executing`, capturar baseline de invalidação e
   executar a closure com um contexto de tracking próprio, somente se o nó
   ainda estiver vivo e o runtime saudável;
6. revalidar a vida do reader e das sources, preparar a reconciliação sem
   publicá-la e restaurar contextos de tracking/owner;
7. no memo, comparar resultado e anterior sem tracking;
8. revalidar o reader, concluir a reconciliação, publicar o resultado e invalidar
   observers diretos se o valor mudou;
9. concluir como `Clean` somente se não houver falha ou invalidação não
   consumida das sources realmente lidas, conforme seção 4.4.

Observers `Check` de um memo que mudou tornam-se `Dirty`. Seu subgrafo já deve
estar marcado; não propagar tudo novamente sem necessidade. Se uma execução
lazy ocorrer antes do flush, effects já enfileirados continuam representando
o trabalho pendente.

Se o valor for igual, manter o valor anterior e não propagar mudança de valor.
A reconciliação de dependências ainda é obrigatória: resultado igual não
significa dependências iguais.

Durante `Executing`, o memo transfere sua célula anterior para os recursos da
execução. Dois slots tipados reutilizáveis alternam valor atual e candidato;
igualdade mantém o slot anterior, mudança publica o candidato, e falha restaura
o cache somente se a mesma geração ainda existir. Os slots contêm `Option<T>`:
retirar o valor para release encerra o borrow antes de executar Drop sob guard.
A função de release tipada usa `needs_drop::<T>()`; erasure exige validação do
TypeId da célula. Publicação estável não aloca um Rc novo por atualização;
o próprio código de T pode alocar. Leituras cíclicas continuam proibidas.

O guard de avaliação salva owner, reader e profundidade; scopes aninhados de
batch/flush/disposal mantêm seus próprios guards completos. Execution owners
com nodes, children e cleanups vazios renovam a geração sem instalar contexto
de disposal: não podem invocar user code. O overflow de disposal continua
checked antes da renovação, e owners não vazios usam o teardown protegido.

A entrada de atualização de memo funde validação de kind/fase/clean e transição
para Checking numa janela de borrow sem callbacks. A checagem de operação
precede essa janela; só IDs internos do runtime dispensam reconstrução de sua
identidade. Entradas ordinárias de avaliação continuam validando identidade.
Clean retorna antes de consumir profundidade; ciclo/limite, guards e revalidação
após user code permanecem. O Drop do guard não é desarmado pelo validador.

### 4.4 Revisões de leitura e conclusão

Separar a geração do slot (vida), a revisão de freshness (consistência da
leitura), a revisão do valor publicado e a sequência da rodada (scheduling).
A revisão do valor só avança em publicação diferente; no signal ela pode ser
a própria revisão de freshness. No memo ela não avança por mera marcação.
Cada aresta guarda a revisão do valor efetivamente consumido na primeira
leitura. Ao verificar `Check`, atualizar a source e comparar essa revisão:
mudança torna o reader `Dirty`, mesmo se a source já estava `Clean` ou mudou
antes de a aresta nova ser publicada. Sem essa comparação, uma reinvalidação
detectada apenas na conclusão poderia se perder na rodada seguinte.
Contadores são inteiros largos com overflow verificado explicitamente,
independentemente do perfil Cargo.
Esgotamento de revisão/rodada envenena o runtime com `CounterExhausted`; não
reiniciar epochs com scan global nem permitir wrap.

A revisão de freshness avança:

- no signal, a cada publicação considerada diferente pelo comparador;
- no memo, ao sair de `Clean`, ao passar de `Check` para `Dirty`, ao publicar
  valor diferente e ao receber nova invalidação enquanto `Executing`, mesmo
  se já estiver `Dirty`;
- disposal é detectado pela geração/vida, não por acesso a uma revisão morta.

Marcação repetida de memo não executando e já suficientemente marcado não
precisa avançar a revisão: ele não pode oferecer uma nova leitura fresca sem
antes ser atualizado. Uma leitura rastreada captura a revisão **depois** da
atualização lazy. Guardar a revisão da primeira leitura por source na execução;
releituras não a substituem, pois a callback pode já ter usado o valor anterior.
Essa metadata pode ficar nos registros de tracking/arestas, sem tabela global.
Não copiar a lista antiga de sources na verificação: guardar um cursor, obter
um ID por borrow curto e revalidar o reader após cada atualização que executou
código de usuário. O callback de cleanup pode ter descartado o reader; nenhum
cursor ou referência emprestada autoriza acesso posterior ao slot morto.

Preparar a reconciliação significa manter IDs/revisões e o prefixo estável no
contexto existente, não copiar todo o grafo nem alocar um log de transação. No
memo, nenhuma alteração de suas arestas é publicada antes de compute e
comparador concluírem com sucesso. O commit interno não executa callbacks;
valores/closures removidos são liberados só depois de terminar suas mutações.

Para effects, as arestas antigas continuam servindo à marcação durante a
callback, mas não decidem sozinhas a necessidade de reexecução. Na conclusão:

1. reader descartado: abandonar o commit, conforme seção 7.1;
2. falha: não publicar `Clean`; aplicar seção 8.2;
3. para cada source realmente lida, comparar vida e revisão capturada com o
   estado atual, aproveitando a travessia de reconciliação;
4. se todas coincidem, concluir `Clean`, mesmo se uma source antiga não mais
   lida tiver provocado marcação durante a callback;
5. source viva divergente: manter `Dirty` se signal mudou ou há mudança direta
   confirmada; caso contrário, manter ao menos `Check` e enfileirar para a
   próxima rodada, permitindo equality cut na próxima verificação;
6. source lida e depois descartada: manter `Dirty` e enfileirar, sem reconstruir
   a aresta morta. Uma execução posterior pode abandonar o handle ou falhar.

Não atualizar memos adicionais só para confirmar revisões na conclusão: isso
anteciparia trabalho lazy. Revisão divergente é evidência conservadora de
invalidação, não prova de mudança de valor. A igualdade das sources pode limpar
um effect `Check` na próxima verificação sem executar sua callback novamente.
Effects sem sources não repetem apenas por terem escrito em outros signals.

Memos puros não podem provocar writes/disposal, inclusive em outro runtime.
Assim, invalidações esperadas de sources durante `Checking` ou ao atualizar uma
source antes de registrar sua leitura são consumidas na avaliação atual. Um
memo só publica como confiável um resultado cujas sources capturadas estejam
vivas e frescas; violação inesperada disso é falha de avaliação, não retorno de
snapshot antigo nem retry ilimitado.

## 5. Tracking dinâmico e arestas

Cada computação mantém `sources` na ordem da primeira leitura. Cada fonte mantém
`observers`. Cada aresta registra os índices necessários para remoção dos dois
lados.

Cada execução possui um contexto:

```text
TrackingContext {
    reader,
    execution_epoch,
    stable_prefix_cursor,
    diverged,
    new_sources,
    seen_sources,
    read_revisions, // freshness + published value revision on first read
}
```

O runtime mantém uma pilha desses contextos. Quando `D` lê `C` e precisa executar
`C`, salvar/restaurar o contexto de `D`. Depois da atualização de `C`, registrar
a leitura de `C` em `D`; não registrar as sources internas de `C` em `D`.

Fast path:

- se a próxima source corresponde à posição antiga e ainda não houve
  divergência, avançar o cursor sem reescrever a aresta;
- ao divergir, acumular a nova sequência sem duplicatas;
- ao concluir, preservar o prefixo estável, remover arestas antigas do sufixo
  e inserir as novas, mantendo a ordem da primeira leitura.

**Deduplicação deve distinguir execuções aninhadas.** Um único marcador
`last_seen_epoch` por fonte pode ser sobrescrito por um memo aninhado e deixar
passar uma leitura duplicada no reader externo. Usar um conjunto por contexto
ou uma estrutura equivalente que preserve a informação dos readers suspensos.
É uma otimização a medir, não uma justificativa para arestas duplicadas.

Começar com busca linear em um pequeno buffer por contexto para deduplicar
readers de baixo fan-in; ao ultrapassar um limiar medido, promover para conjunto
reutilizável. O fast path deve estabelecer que a read é nova **antes** de avançar
o cursor, por deduplicação ou por correspondência com a próxima source única:
`A, A, B` não deve divergir da sequência deduplicada `A, B`. Registrar revisões
também no prefixo estável; preservar arestas não dispensa verificar freshness.
Não exigir hash table nem alocação nova em toda execução. Buffers suspensos
pertencem ao respectivo contexto e não podem ser reutilizados por readers
aninhados. O custo de busca linear é `O(k²)` para `k` sources distintas até o
limiar; após promoção, esperar custo amortizado `O(n)` com hashing para n
tentativas de read (inclusive duplicatas), além de limpeza do conjunto e
reconciliação. Hashing fornece custo esperado, não garantia de pior caso.

Na implementação otimizada, corresponder à próxima source live do prefixo
estável dispensa busca/hash: sources comprometidas são únicas e ordenadas, e
teardown somente marca tombstones sem deslocar arestas vivas. Duplicatas passam
pelo fallback sem causar divergência nem substituir a primeira revisão. No
modo hash, o conjunto pode cobrir apenas um prefixo das reads únicas; antes do
fallback, inserir o sufixo ainda não coberto. Isso preserva o tracking de readers
suspensos, sem marcador global ou novos campos por nó. Reads inteiramente
estáveis e sem duplicatas processam as reads em `O(k)` sem scans de dedup;
inicializar/finalizar tracking pode também limpar capacidade retida de um
`HashSet` usado anteriormente. Verificação e reconciliação continuam obrigatórias.

Ao usar `swap_remove` em observers, atualizar o back-index da aresta movida.
Não usar `swap_remove` na sequência pública de sources se isso alterar sua
ordem de leitura. Prever disposal durante processamento de filas sem referências
emprestadas a vetores que podem ser modificados.

## 6. Flush, rodadas e reentrância

O runtime é single-thread, mas continua precisando tratar reentrância.

### 6.1 Ordem e filas

- Rodada: snapshot da fila pendente, com deduplicação.
- Ordenar effects pelo depth do owner; usar sequência de criação monotônica
  como desempate determinístico, independente de reenfileiramentos.
- Depth de owner não é ordenação topológica do DAG de dependências.
- Atualizar memos por demanda durante a verificação/leitura.
- Verificar geração de cada effect antes de executá-lo.
- Effects criados durante a rodada e effects invalidados após seu processamento
  entram na próxima rodada. Invalidações anteriores ao processamento são
  absorvidas pela execução atual; durante execução, aplicar a seção 4.4.
- Effect já processado não executa novamente na mesma rodada.
- `flush` reentrante retorna erro; o host não deve entrar novamente pelo hook.

Um pai que descarta filhos torna obsoletos os handles desses filhos nas filas.
Ignorá-los ao processar; não executar um filho pertencente à execução anterior.

### 6.1.1 Transições da fila

`Pending` designa o buffer a consumir na próxima abertura de rodada (antes de
um flush, é a primeira rodada). `Current(round)` designa um único ticket vivo
no snapshot atual; a fase de avaliação distingue espera de execução.

| Situação ao invalidar effect | Ação |
| --- | --- |
| `None`, fora de avaliação | Inserir um ticket em `Pending`. |
| `Pending` | Atualizar validade, sem inserir outro ticket. |
| `Current(round)`, aguardando | Atualizar validade; o ticket atual absorve a mudança. |
| `Current(round)`, `Checking`/`Cleaning` | Consumir mudança antes da closure; não inserir outro ticket. |
| `Current(round)`, `Executing` | Registrar evidência; decidir próxima rodada pela seção 4.4. |
| Já processado na rodada, agora `None` | Inserir em `Pending`, nunca reinserir no snapshot atual. |
| Descartado | Cancelar pertença lógica; tickets físicos obsoletos são ignorados. |

Ao abrir a rodada, trocar os dois buffers, filtrar tickets obsoletos e promover
os vivos para `Current(round)`. Ordenar uma vez com `sort_unstable_by_key` usando
`(owner_depth, creation_sequence)`: a chave total mantém determinismo sem exigir
sort estável ou buffer auxiliar de ordenação. Depth é armazenado no owner;
owners não são reparentados na API inicial.

Ao concluir o processamento, retirar a pertença `Current` e, se ainda houver
invalidação relevante, criar exatamente um ticket `Pending`. Um effect `Check`
que ficou `Clean` por equality cut consome o ticket sem contar como execução
de callback. Registrar separadamente última rodada processada e última rodada
em que a callback executou. Tickets carregam apenas IDs, não `Rc` de nós.

Manter contadores de tickets **vivos** por buffer para `has_pending_effects()`
em `O(1)`, atualizados inclusive no disposal. Não procurar duplicatas no vetor
nem percorrer o grafo para responder à consulta. Tickets mortos são compactados
na abertura/saída do flush ou por manutenção amortizada do próprio buffer:
quando ultrapassarem um limiar mínimo e uma fração definida dos tickets físicos,
filtrar os obsoletos. Isso limita retenção após ciclos de criar/descartar sem
flush, sem impor remoção linear em todo disposal. Cancelar pertença lógica é
`O(1)`, mas um disposal que atinge o limiar pode disparar filtragem `O(Q)` de
tickets físicos: a garantia da manutenção é amortizada, não `O(1)` por disposal.
Remover buffers vazios grandes
em pontos frios conforme política de retenção medida.

Compactação durante callbacks só alcança `Pending`; o snapshot `Current` não
é reorganizado enquanto seu cursor está ativo. Cancelamento no snapshot muda
apenas metadata/contadores; tickets físicos são ignorados e drenados ao sair.

### 6.2 Escritas dentro de effects

São permitidas, com writes imediatos e marcação normal. Não podem ser perdidas
porque o effect já estava `Dirty` ou porque sua execução acabou escrevendo
`Clean` sobre uma invalidação nova.

Registrar nova invalidação/versionamento durante `run`. Ao terminar, manter o
effect pendente para a próxima rodada quando necessário. A implementação
inicial usa revisões de leitura, conforme seção 4.4, não arestas provisórias.
Isso cobre signals novos ainda sem aresta publicada e memos lidos antes de
um write a montante, sem mutar listas de observers a cada leitura. Limpar o
effect sem essa verificação pode esquecer um write.

Escritas vindas de outros effects não podem alterar a lista de processamento
atual de modo a permitir execuções ilimitadas na mesma rodada.

### 6.3 Limite da garantia de uma execução

Garantia adotada:

- cada **effect** executa no máximo uma vez por rodada;
- cada **memo** executa no máximo uma vez por versão estável das suas
  dependências; leituras posteriores sem nova invalidação reutilizam o valor;
- com writes imediatos dentro de effects, um memo pode precisar executar
  novamente na mesma rodada para fornecer uma leitura fresca.

Exemplo: um effect lê `M(A)`; outro altera `A`; um terceiro lê `M(A)` na mesma
rodada. Não é possível garantir simultaneamente valor fresco e apenas uma
execução de `M` nessa rodada. Se for necessário impor uma execução por rodada
a todas as computações, o contrato deve mudar para writes diferidos/snapshots,
com semântica explícita de leitura. Não misturar as duas promessas.

### 6.4 Limites de estabilização

`FlushOptions` deve incluir ao menos um limite de rodadas. Prever limite de
trabalho ou possibilidade de yield entre rodadas para integrações com budget
de frame. Não interromper arbitrariamente uma closure em andamento.

Na primeira versão, limites de trabalho/deadline são verificados entre rodadas;
uma rodada iniciada termina, podendo ultrapassar o budget pelo custo dessa
rodada. O relatório expõe esse limite cooperativo, não promete tempo máximo.
Não consultar relógio por aresta/read no caminho quente. Ao parar entre rodadas,
só o buffer `Pending` precisa ser preservado, sem reordenar um snapshot parcial.
Limite de rodadas indica `RoundLimit`/loop provável, não prova de ciclo; yield
por budget indica `Yielded`. `Stabilized` requer zero tickets vivos. Nenhum
desses três resultados envenena o runtime.

Ao exceder um limite:

- preservar o estado pendente e a integridade do grafo;
- devolver relatório distinguindo estabilização, yield e loop provável;
- não publicar nós como `Clean` apenas para encerrar;
- o host decide reagendar, apresentar diagnóstico ou interromper a aplicação.

### 6.5 Permissões e reentrada de callbacks

Restrições são cumulativas: entrar em `untrack`, `batch`, outro owner ou outro
runtime nunca amplia as permissões do contexto externo. Guards de capacidade
por thread aplicam a restrição também a chamadas explícitas em outro runtime;
não servem para resolver handles ou armazenar um runtime global. Acesso ao
runtime permanece explícito. Fora de callbacks restritas, o caminho normal
verifica apenas um estado compacto de capacidade; paths de erro são frios.

| Contexto | Leituras reativas | Writes | Criar nós/owners ou registrar cleanup | Disposal | Flush |
| --- | --- | --- | --- | --- | --- |
| Host, sem operação ativa | Sim | Sim | Sim, com owner explícito | Sim | Sim |
| Memo | Sim | Não | Não | Não | Não |
| Effect | Sim | Sim | Sim, no owner de execução vivo | Sim | Não |
| `Updating` (callback após clone) | Sim | Sim | Sim, com owner vivo | Sim | Não |
| Leitura emprestada / `T::clone()` | Não | Não | Não | Não | Não |
| Comparador | Não; usar argumentos fornecidos | Não | Não | Não | Não |
| Cleanup | Sim, sem reader implícito | Sim, se runtime saudável | Não | Sim, se alvo não estiver em descarte | Não |
| Hook / destrutor executado pelo runtime | Não | Não | Não | Não | Não |

Writes/mutações estruturais proibidos por pureza retornam `WriteForbidden` ou
`MutationForbidden`; demais reentradas proibidas retornam `Reentrancy`.
Comparadores não leem o grafo nem atualizam memos: isso evita dependências
ocultas, ciclos de comparação e conflito entre comparação e commit. A matriz
não impede efeitos externos que Rust não consegue interceptar; a pureza destes
continua obrigação do usuário. Leituras de outro runtime não são sources
rastreáveis do reader atual: sob tracking, retornar `WrongRuntime`; sob
`untrack`, admitir leitura explícita, sem promessa de invalidação cruzada.

Cleanups de execução de um memo, se existirem por infraestrutura interna, não
podem ampliar sua capacidade de pureza. A API pública não permite que memos
registrem recursos/cleanups. Cleanups de effects recebem o runtime como
argumento e não herdam o reader da computação externa suspensa.

Destrutores liberados pelo runtime não podem reentrar em APIs do grafo. Não
garantir essa interceptação quando o último `Rc<T>` é liberado pelo próprio
usuário fora do runtime, ou quando um clone proprietário é liberado pelo host;
destrutores desses valores devem ser livres de efeitos
reativos. Para liberação que precisa escrever em signals, usar cleanup explícito,
não `Drop` do valor. Essas regras permitem publicar/remover dados em borrows
curtos sem um protocolo arbitrário de destrutores reentrantes.

## 7. Owners, disposal e cleanups

Separar dois conceitos:

1. owner de vida do nó, por exemplo a existência de um componente;
2. owner do conteúdo criado numa execução, descartado antes da reexecução.

O nó da computação continua existindo enquanto seus filhos da execução anterior
são removidos. O owner externo permite descartar toda a computação.

Disposal deve:

- tornar o escopo inacessível/rejeitar novas inscrições antes dos callbacks;
- descartar descendentes antes dos cleanups do pai;
- executar cleanups em ordem reversa ao registro, com descendentes antes do pai;
- remover as arestas dos dois lados;
- invalidar gerações e liberar slots/closures/valores;
- deixar entradas obsoletas nas filas inofensivas, sem reter indefinidamente
  closures ou valores associados.

Se um nó sobrevivente dependia de um nó descartado, remover a aresta e marcar
a computação sobrevivente como `Dirty`. Na reexecução ela pode deixar de ler a
fonte; se ainda ler o handle descartado, retornar erro. Não manter um memo
`Clean` com um valor antigo após sua dependência deixar de existir.

A remoção lógica é imediata dos dois lados. O reader sobrevivente mantém um
slot morto com somente IDs/revisões (`live = false`), sem payload ou referência
à source; back-indices das arestas vivas não mudam. A reconciliação bem-sucedida
remove o sufixo antigo, inclusive tombstones, e publica apenas arestas vivas.
Em runtime saudável, tombstones implicam reader `Dirty`; snapshots e contagem
de arestas os excluem. Disposal do próprio reader também os elimina.

Assim, descartar individualmente k sources de um reader sem avaliações entre
disposals não faz k compactações: remover as arestas custa O(k), mais marcação,
teardown e código do usuário. A reconciliação custa O(m − p + k') para m slots
antigos físicos (incluindo tombstones), prefixo preservado p e k' reads únicas
novas. Avaliações intercaladas continuam pagando seu próprio trabalho. Slots
mortos podem persistir até avaliação/disposal; não há redução de capacidade
retida nem promessa de disposal completo O(1).

Cleanups não recebem tracking implícito. Podem liberar recursos externos e,
conforme o contrato da API, escrever em signals ainda vivos; essas escritas são
agendadas normalmente, sem flush reentrante. Não permitir criar filhos no owner
que está sendo descartado. O disposal precisa continuar consistente mesmo se
um cleanup falhar ou entrar em panic.

Effects são adequados para criar escopos dinâmicos e recursos com cleanup.
Memos devem derivar valores; construir/dispor recursos com efeitos colaterais
dentro deles viola a premissa de pureza, mesmo que a infraestrutura suporte
owners em ambos os tipos de computação.

### 7.1 Disposal durante uma execução

Owners e nós têm estado de vida `Alive`/`Disposing`; apenas `Alive` aceita
operações novas. Marcar todo o subtree a remover como `Disposing` antes do
primeiro callback, usando travessia explícita do próprio subtree, sem scan do
grafo. Desvincular nós/arestas e tickets lógicos antes de chamar cleanups ou
liberar closures/valores; preservar apenas os recursos necessários ao teardown.
Dependentes externos são invalidados pela marcação normal. Sources dentro do
subtree já indisponível não podem ser lidas por seus cleanups.

`shutdown` marca **todas** as roots, owners e nós como `Disposing` antes de
desvincular qualquer aresta. Assim não invalida, reenfileira ou avança revisões
de readers em outras roots que também serão removidas. A ordem de teardown
permanece a anterior: ordem inversa do vetor atual de roots (que pode mudar por
`swap_remove`), e BFS inversa dentro de cada root, com cleanups LIFO e liberação
de payloads após os cleanups do respectivo owner. Todo o grafo é destacado antes
do primeiro callback; failures/panics continuam drenando recursos independentes
e preservando a primeira falha. Shutdown reentrante/idempotente encontra roots
vazias. A travessia usa O(owners vivos) IDs temporários, sem scan de slots livres.

É permitido que um effect descarte a si próprio ou seu owner. Rust não
interrompe a callback: ela continua, mas o owner de execução deixa de aceitar
inscrições. Ao retornar, o guard revalida `(runtime, slot, generation, life)`
antes de restaurar a closure, reconciliar ou publicar. Se descartado:

- não restaurar closure em nenhum slot, mesmo se o índice já foi reutilizado;
- não inserir arestas, publicar valor ou reenfileirar o effect;
- liberar closure/contexto temporários fora do borrow, sob guard de destrutor;
- restaurar o contexto externo; o flush conta a callback que chegou a executar.

Aplicar a mesma revalidação depois de `Cleaning`, antes de entrar na closure.
Se um cleanup descartou a computação ou envenenou o runtime, não executar a
closure retirada. Acesso ao owner atual descartado é erro, não fallback para
um owner externo/global.

Não reciclar buffers de contexto ainda ativos até a callback retornar. Novas
arestas só são publicadas após validar reader e sources vivos. Se a callback
descartou uma source após lê-la, aplicar a reinvalidação da seção 4.4, sem
inserir uma referência morta. Erro não tratado pela callback continua sujeito
à política de falha, mesmo se ela descartou o próprio nó.

Disposal público é idempotente para handle do mesmo runtime já descartado:
retorna `AlreadyDisposed`/no-op, sem tocar eventual ocupante novo do slot.
Runtime errado permanece erro. Disposal de alvo já `Disposing` é no-op,
evitando recursão de teardown; outros alvos vivos podem ser descartados por
cleanup. Owners não conhecem todos os handles copiados: apenas slots e arestas
do subtree são percorridos. A geração basta para rejeitar cópias antigas.

## 8. Ciclos, erros e panic safety

### 8.1 Ciclos

Detectar acesso a computação já em avaliação, tanto na verificação de sources
quanto na execução da closure. Retornar erro com o caminho quando houver
diagnósticos habilitados. Não detectar apenas recursão de `run`: um ciclo pode
aparecer durante a travessia de nós `Check`.

Separar ciclo de dependência de loop de effects que escrevem em signals:

- ciclo de dependência: erro de avaliação;
- loop de effects: limite de rodadas/trabalho no flush.

Um DAG pode ter ramos longos. A implementação inicial pode usar recursão com
guard de profundidade e erro distinto de ciclo. Prever uma máquina iterativa
para atualização se profundidade/custo exigirem; marcação já começa iterativa.

### 8.2 Política de falha

- Escritas dentro de memos/comparadores são proibidas.
- Handle obsoleto não vira leitura de outro nó nem valor default.
- Runtime errado é erro explícito.
- Não marcar um valor derivado que falhou como confiável.
- A primeira implementação envenena o runtime nas falhas de avaliação definidas
  abaixo, exigindo disposal/recriação; erros de uso locais não o envenenam.
- Recuperação granular de computações deve ser um contrato posterior, não
  presumida após `catch_unwind`.

A primeira versão adota `Healthy`/`Poisoned(first_failure)`, sem reset:

| Ocorrência | Resultado |
| --- | --- |
| Handle inválido, runtime errado, ausência de owner, conflito de update, operação proibida ou flush reentrante | Erro local; nenhuma mutação da operação rejeitada; runtime continua saudável. |
| Erro retornado por compute/effect/cleanup, ciclo ou limite de profundidade durante avaliação | Envenenar, interromper avaliação/flush e não publicar resultado confiável. |
| Panic em código chamado pelo runtime, incluindo clone/comparador/hook/destrutor | Envenenar; com unwind, restaurar invariantes antes de propagar o primeiro panic. |
| Esgotamento de revisão/rodada/sequência ou violação interna de invariantes | Envenenar; erro fatal explícito, nunca wrap/default. |
| Limite de rodadas ou yield | Preservar trabalho; runtime continua saudável. |

Um erro local tratado dentro da closure não causa poison. Se a closure o
propaga como seu resultado `Err`, torna-se falha de callback. Erros de avaliação
já envenenam o runtime ao ocorrerem, mesmo se uma closure externa tentar
capturá-los; não há recuperação granular implícita.

Esgotamento de geração de slot apenas aposenta esse slot, conforme seção 2.3;
esgotamento de identidade impede criar um runtime novo, sem envenenar outros.
Isso não exige tabela histórica de handles mortos nem scan de runtimes.

Após poison, reads (inclusive signals), writes, criação e flush retornam
`Poisoned`. Cópias/clones proprietários já entregues continuam válidos, sem representar
estado reativo atualizado. São permitidos apenas diagnóstico, disposal e
`shutdown()` explícito de todos os roots. Não chamar o hook nem executar effects
pendentes. As filas não retêm closures; disposal continua cancelando tickets.
O relatório da operação que falhou identifica a primeira falha e trabalho
remanescente; este não é executável até recriar o runtime.

`shutdown()` bloqueia inscrições e descarta todos os roots ainda vivos, sem
reativar o runtime. O host deve usá-lo/dispor roots antes de liberar sua última
fachada para garantir cleanups explícitos. A liberação final do armazenamento
libera valores/closures, mas não executa effects nem substitui esse contrato
de teardown por callbacks implícitas em `Drop`.

### 8.3 Protocolo de unwind e teardown

Com `panic=unwind`, executar callbacks sob fronteira de `catch_unwind`; guards
restauram contextos e a função restaura/desvincula recursos antes de
`resume_unwind`. `AssertUnwindSafe`, se necessário, só é usado depois de
documentar que o runtime será envenenado, não para alegar recuperabilidade da
closure. Nunca retornar `Clean` porque o panic foi capturado.

Ao falhar cleanup, guardar a primeira falha, envenenar e continuar removendo
arestas, slots e recursos. Os cleanups restantes são retirados antes de invocar
cada callback e tentados uma única vez, em ordem reversa, com captura individual
de erro/panic; writes neles já retornam `Poisoned`. Registrar falhas secundárias
sem substituir a primeira. Concluir teardown estrutural antes de devolver erro
ou propagar o primeiro panic. Não retentar um cleanup que falhou.

Guards em `Drop` durante unwind só restauram metadata e transferem recursos
pendentes ao teardown; não executam callbacks de usuário. A fronteira externa
drena esses recursos após capturar o panic original. A liberação de valores e
closures também ocorre fora de borrows e com captura individual quando há
unwind. Payloads de panic são recursos de usuário: evitar descartá-los durante
outro unwind; um payload secundário cujo `Drop` entra em panic pode exigir
retenção excepcional documentada. Dois panics internos ao mesmo destrutor
durante unwind podem abortar conforme Rust; o runtime não promete impedir isso.

Em `Err`/panic de compute ou effect, descartar os registros provisórios de
tracking, preservar as arestas anteriores ainda vivas e manter o nó inválido.
Se o slot original continua vivo, devolver a closure retirada ao nó para
liberação posterior pelo disposal, nunca para reexecução pós-poison. Caso
contrário, liberá-la pelo teardown. A reconciliação parcial não é publicada
como uma execução bem-sucedida. Erro/panic em comparador de memo também impede
publicar o candidato; reter o valor anterior não o torna confiável.

Testes de unwind exigem binário efetivamente compilado com `panic=unwind`.
Os perfis `fast-dev` e `release` atuais usam `panic=abort`: mantê-los para
desenvolvimento/performance conforme o projeto, mas executar a suíte de unwind
no perfil de testes com configuração unwind verificada. Em abort não há
restauração pós-panic; falhas representadas por `Result` seguem o protocolo
normal. Não comparar benchmarks unwind com baseline abort sem indicar a diferença.

Mesmo propagando panic ao chamador, guards devem restaurar tracking, batching,
owners e flags de execução. Closures retiradas com `take()` não podem desaparecer
silenciosamente; devolver ou descartar de forma definida. Cleanups e destrutores
devem rodar fora de borrows internos. A implementação deve especificar o que
acontece com os cleanups restantes se um cleanup entrar em panic, evitando um
segundo panic durante unwind. Não há garantia de cleanup em `panic=abort`.

## 9. Estratégia Rust e memória

### 9.1 Começar seguro

Priorizar um `Runtime` explícito com handle clonável e interior mutability
segura. Uma fachada `thread_local!` pode oferecer ergonomia de contexto atual,
mas não deve ser a única maneira de construir/testar o runtime nem esconder
qual runtime possui um handle.

`compute.take()` sozinho não resolve borrow. Antes de executar código do
usuário, liberar qualquer `RefMut<Runtime>` e não manter referências a elementos
de seus vetores. Depois da callback, adquirir novo borrow para reconciliar.
Aplicar o mesmo princípio a comparadores, hooks, cleanups e destrutores.

Aplicar também a `T::clone()`: obter a célula tipada sob borrow curto do grafo,
liberar o grafo e então clonar T sob borrow imutável do valor e capabilities
de leitura emprestada. Liberar esse borrow antes da callback de edição.
Não mover/drop incidentalmente o último `Rc`, uma closure substituída ou um
buffer contendo valores de usuário sob borrow. Buffers de IDs/revisões não
executam código de usuário e podem ser manipulados nas seções internas.

`UnsafeCell`, raw pointers e `unsafe impl Send/Sync` não são soluções iniciais
para conflitos de borrow. Qualquer otimização futura exige invariantes
documentadas, testes de segurança e benefício medido.

### 9.2 Armazenamento

Prever:

- slots geracionais e lista de slots livres;
- metadata quente: estado, geração, flags e observers;
- revisões compactas e pertença lógica a fila junto à metadata de acesso;
- metadata fria: sources, closure, valor e owner;
- pilhas e filas reutilizáveis;
- pequenos buffers inline para poucas dependências, se medições justificarem;
- descarte explícito de buffers grandes não mais necessários.

Não prometer zero alocação absoluto: criar nós, descobrir novas arestas ou
aumentar buffers pode alocar. O objetivo é evitar alocações recorrentes em
marcações/flushes de grafos estáveis depois do aquecimento.

A separação hot/cold é uma hipótese de layout, não obrigação de introduzir
indireção extra por nó antes de medir. Baseline seguro usa slots, vetores
reutilizados e células `Rc<RefCell<Option<T>>>`; comparar array de structs com separação de
metadata usando os mesmos grafos. Não alocar um owner de execução novo em toda
rodada: reutilizar um escopo vazio do nó após invalidar filhos antigos, sem
revalidar handles de conteúdo descartado. IDs de owners de conteúdo públicos
precisam de nova geração quando o escopo é renovado.

Em grafo estável, reutilizar arestas do prefixo e buffers de tracking; checar
revisões na própria passagem de conclusão, sem segunda cópia de sources. A
reconciliação que remove sufixos pode precisar de acesso adicional às arestas,
mas não deve reconstruir o prefixo estável. Uma computação pode alocar dentro
de T; não confundir essa alocação com overhead das filas/marcação. O runtime
reutiliza dois slots de valor por memo. Equality cut retém o valor anterior e libera o candidato
fora dos borrows.

### 9.3 Igualdade e compartilhamento explícito

`PartialEq` é conveniente para valores pequenos. Para coleções grandes, usar
compartilhamento explícito via `Rc<T>`, identidade/revisão e comparadores explícitos.

Não usar tamanho, contagem de pontos ou igualdade aproximada como substituto
da identidade de um stroke/documento. Não mutar dados por interior mutability
sem realizar uma operação reativa que publique nova revisão.

Para floats, a política precisa ser explícita: `NaN != NaN` em `PartialEq`,
enquanto comparação por bits ou tolerância possui outra semântica. O runtime
não escolhe tolerância geométrica implicitamente.

## 10. Invariantes verificáveis

1. Signals vivos permanecem `Clean` e suas leituras refletem o último write.
2. Leituras de memos retornam valores atualizados ou erro explícito.
3. Fora das seções de execução/reconciliação, a invalidação de uma computação
   está representada nos seus observers transitivos por `Check`/`Dirty` ou
   pela reinvalidação agendada de uma computação em execução.
4. Um effect pendente possui uma representação lógica única nas filas; estado
   `Dirty` sozinho não prova que ele foi enfileirado.
5. Effects não executam duas vezes na mesma rodada.
6. Sources refletem a ordem da primeira leitura na última execução bem-sucedida,
   sem duplicatas.
7. Cada aresta viva tem representação bidirecional e índices coerentes.
8. Nenhum handle com geração antiga acessa um slot reutilizado.
9. Callbacks de usuário não executam enquanto o runtime mantém borrow interno
   incompatível com reentrada permitida.
10. Falha ou invalidação não consumida de source realmente lida durante execução
    não é sobrescrita por `Clean`.
11. Disposal não deixa computações sobreviventes confiando em fontes removidas.
12. Equality cut preserva o valor, mas não impede atualizar dependências.
13. Revisões de valor consumidas nas arestas permitem detectar mudança ocorrida
    antes de publicar uma dependência nova, mesmo com a source já `Clean`.
14. Uma invalidação consumida em `Checking`/`Cleaning` não cria execução extra;
    conclusão considera as sources realmente lidas, não dependências abandonadas.
15. Reader descartado durante callback nunca restaura closure/valor/arestas
    num slot morto ou reutilizado.
16. Contadores de tickets vivos concordam com pertença lógica; tickets mortos
    não mantêm `has_pending_effects()` verdadeiro nem retêm recursos de nós.
17. Guards de capacidade são cumulativos entre runtimes e não são removidos por
    `untrack`; poison nunca é revertido por uma callback externa.

## 11. Complexidade e limites das afirmações

| Operação | Custo esperado |
| --- | --- |
| `set` com valor igual | Comparação do valor; nenhuma propagação. |
| Marcação | Arestas dos nós cuja marcação ainda precisa propagar, além da enumeração dos observers diretos. |
| Verificação de `Check` | Sources visitadas até a primeira invalidação confirmada. |
| Reexecução | Custo da closure, igualdade, tracking e reconciliação. |
| Ordenação de effects | `O(E log E)` se usar sort para `E` effects da rodada. |
| Consulta de effects pendentes | `O(1)`, por contadores de tickets vivos. |
| Cancelamento lógico de ticket | `O(1)` para metadata/contadores; não inclui manutenção/compactação do buffer. |
| Validação de revisões de effect | `O(k)` para as k reads únicas capturadas; não inclui remoção/publicação de arestas. |
| Conclusão completa de effect | `O(k + m − p)`: validação/recuperação do prefixo e reconciliação; m inclui tombstones antigos, p é o prefixo preservado. Deduplicação durante a callback tem custo próprio. |
| Compactação de filas | `O(Q)` dos tickets físicos do buffer, amortizada por limiar de obsolescência; sem scan de nós. |
| Disposal | Nós, slots de sources (inclusive tombstones) e arestas/cleanups do escopo removido, mais invalidação de dependentes externos e possível compactação `O(Q)` da fila; não é `O(1)` por chamada. |
| Shutdown | Travessia estrutural de owners/nós/arestas e tombstones físicos, sem invalidação de readers entre roots; filas, tracking ativo, callbacks/destructors e allocator têm custos próprios. O(owners vivos) IDs temporários. |
| Flush completo | Soma do trabalho de todas as rodadas: filtrar tickets, ordenar effects, verificar/atualizar memos, callbacks, conclusão/reconciliação, teardown e manutenção de filas. |
| Espaço do grafo lógico de dependências | `O(V + A)` somente para nós reativos e arestas vivas, não para todo o runtime. |

Na conclusão de effect, recuperar o prefixo é integrado à passagem de validação
e compactação das reads; sem source removida não se copiam records sobre eles
mesmos. Isso reduz passagens/stores, não a ordem de crescimento. Desvincular
m − p arestas antigas requer atualizar seus observers (ou consumir tombstones),
mesmo que a nova execução leia somente uma source. Adiar a remoção mudaria o
protocolo de arestas/agendamento e reteria trabalho obsoleto; não é realizado.

### 11.1 Memória lógica e capacidade física

V conta nós reativos e A arestas vivas. Owners, registros de cleanup, reads
capturadas, entradas dos conjuntos de dedup, tombstones, tickets físicos
(inclusive mortos) e pilhas/buffers temporários devem ser somados separadamente,
além de valores, closures e valores proprietários/`Rc` que o usuário ainda mantém.
A expressão `O(V + A)` não limita essas estruturas nem a memória total.
A implementação segura
mantém slots geracionais e capacidades de buffers para reuso: memória física
segue os slots/capacidades alocados (high-water mark), não apenas `V/A` vivos
depois de disposal. Disposal libera valores/closures/arestas, mas não promete
devolver arenas ao allocator; a última facade libera o storage. Consultas
diagnósticas explicitam essa retenção, e filas vazias grandes liberam seus
buffers conforme a política medida em `benches/.log/RESULTS.md`.

`HashSet::clear()` remove entradas sem liberar capacidade. A limpeza de um
conjunto não vazio pode percorrer/resetar armazenamento proporcional à
capacidade retida, mesmo quando a execução seguinte lê poucas sources; conjuntos
já vazios podem ter atalho. Não contabilizar somente o número atual de entradas
ou assumir que `clear()` sempre custa `O(1)`. Isso se aplica à deduplicação de
tracking e, quando habilitado, ao conjunto de execução usado por verification.

### 11.2 Flush e budgets cooperativos

Para cada rodada r, incluir ao menos `O(Q_r + E_r log E_r)` para abrir o
snapshot, mais o trabalho de cada computação e da manutenção ao encerrá-lo.
O custo total soma todas as rodadas; E_r não é a contagem total de nós do grafo.
Atualização recursiva de sources, equality cuts, cleanup e destrutores também
entram nessa soma. Um memo pode executar novamente após novos writes na mesma
rodada, conforme a seção 6.3.

Budgets/deadlines são verificados entre rodadas, não interrompem uma rodada
iniciada ou uma callback. `max_work` conta tickets vivos processados, não
operações internas nem tempo de execução. Portanto não fornece limite rígido
de latência/trabalho por rodada; até uma única callback pode exceder o budget.

### 11.3 Stack e custos externos

Marcação e teardown usam travessias iterativas com buffers explícitos.
Verificação/atualização ascendente de memos continua recursiva, com limite
checado de profundidade de avaliação 512; exige stack suficiente na thread
do host. O limite não torna a travessia iterativa nem garante um consumo de
stack fixo em bytes, especialmente com código do usuário aninhado.

Writes repetidos podam a propagação por nós já marcados, mas não custam zero:
há igualdade, observers diretos e eventual agendamento. Mudanças de dependências
podem exigir reconstrução de um sufixo inteiro. Callbacks/comparadores podem
dominar o custo. Não prometer trabalho constante, ausência de allocations ou
execução única universal por frame.

Glitch-free significa que a leitura de um memo não combina valores derivados
desatualizados para o conjunto atual de writes. Não significa isolamento entre
effects que fazem writes sequenciais. O runtime não oferece transações por
usar batching.

## 12. Integração com GUI, input e GPU

```text
input normalizado / mensagens externas
                  │
        handlers → signals → marcação
                  │
       host agenda callback de frame
                  │
                 flush
                  │
      effects alteram props retidas
                  │
      layout / hit-test / paint dirty
                  │
       renderer prepara/submete GPU
```

O runtime não agenda frames diretamente. O host combina:

- effects pendentes;
- layout/paint/hit-test dirty;
- animações e deadlines;
- resize/expose e estado da superfície;
- preparação, retries e validação GPU pendentes;
- notificações de conclusão e manutenção de caches.

Ausência de effects não significa ausência de trabalho gráfico. Uma conclusão
GPU deve poder acordar o host sem movimento de mouse. Uma leitura lazy também
pode atualizar um memo fora de um frame/flush.

O contrato de `effect_queue` não é fila de eventos de input. Coalescer pedidos
de flush não pode descartar amostras de desenho: o integrador acumula todas as
amostras e publica snapshots/revisões, atualizando a apresentação conforme a
cadência disponível. Effects representam estado atual, não entrega de cada
evento intermediário; eventos que exigem entrega individual precisam de fila
própria.

Um único owner não deve misturar vida de componente, validade de cache e vida
de recursos GPU in-flight. Cleanup da GUI pode retirar uma referência lógica,
mas fences e retirement queues do backend continuam responsáveis por liberar
recursos físicos com segurança.

## 13. O que o crate não deve fazer

- Depender de GPUI, Wayland, Vulkan, wgpu, Vello ou Taffy.
- Criar janela, conduzir o event loop ou decidir cadência de apresentação.
- Conhecer documento, stroke, câmera, ferramenta, widget ou layout.
- Serializar o grafo reativo como documento da aplicação.
- Reconstituir input a partir de métricas ou eliminar amostras intermediárias.
- Simplificar geometria, reduzir AA ou alterar igualdade geométrica implicitamente.
- Administrar VRAM, cache de rasterização, fences ou targets gráficos.
- Usar broadcast global como substituto do tracking por leitura.
- Fazer diff de árvores completas para cada alteração de signal.
- Fazer scans globais do grafo a cada write/flush.
- Executar effects sincronamente em `set` ou no hook de agendamento.
- Interpretar `batch` como transação.
- Permitir memos impuros sem um contrato distinto.
- Prometer execução única de todos os nós por frame com writes imediatos.
- Manter owners implícitos imortais ou callbacks após disposal.
- Resolver conflitos de borrow silenciando-os com `unsafe`.

O runtime sozinho não torna fine-grained a compilação de cena ou a pintura.
Essas camadas precisam de identidades estáveis, revisões locais e invalidações
adequadas; pertencem a outros crates.

## 14. Testes obrigatórios

### Correção do grafo

- Diamante: valores consistentes e memo final executado uma vez sem writes novos.
- Equality cut em signal e em múltiplas profundidades de memos.
- Memo lazy atualizado por leitura antes do flush.
- Dependência condicional adicionada/removida com resultado igual ou diferente.
- Ordem de sources alterada e short-circuit na primeira source que confirma mudança.
- Leituras duplicadas, inclusive com avaliação de memo aninhado entre leituras.
- Prefixo estável com `A, A, B`: nenhuma aresta duplicada ou divergência falsa.
- `peek`/`untrack` de memo sujo: sources internas rastreadas, reader externo não.
- Back-indices corretos após remoção e `swap_remove` de observers.
- Nós não afetados não executam nem são visitados na atualização.

### Rodadas e scheduling

- Muitos writes antes do flush: effect enfileirado uma única vez.
- Effect escreve na própria source: reinvalidação não se perde.
- Effect lê source nova e depois a escreve: reinvalidação não se perde.
- Effect altera dependência de outro effect já processado: próxima rodada.
- Effect altera outro effect ainda aguardando: execução atual absorve a mudança,
  sem ticket adicional nem rodada vazia.
- Memo muda durante `Checking`: effect executa uma vez e estabiliza sem rodada
  extra se não houver writes novos.
- Cleanup altera source antes da closure: a nova leitura absorve o write.
- Effect abandona source antiga e a altera: não reexecutar por aresta abandonada.
- Effect lê memo novo, altera uma source e atualiza o memo antes de concluir:
  revisão do valor consumido provoca reexecução se mudou; equality cut evita
  callback extra se o valor final do memo não mudou.
- Primeira leitura, write e releitura da mesma source: não sobrescrever a
  revisão da primeira leitura nem perder a necessidade de estabilização.
- Memo lido, invalidado por effect e lido novamente na mesma rodada: valor fresco.
- Effects criados durante rodada executam na rodada seguinte.
- Batch aninhado, leitura fresca dentro de batch e saída por panic.
- Hook coalescido, chamado sem borrow interno e incapaz de iniciar flush reentrante.
- Flush síncrono pelo hook é rejeitado mesmo sem flush externo ativo.
- Cancelamento de callback pelo host, rearm explícito e troca/remoção de hook.
- Yield/limite de rodadas preserva trabalho e permite reagendamento.
- Ordem determinística de effects com mesmo depth.
- Contadores de tickets vivos após disposal e compactação sem flush; filas
  físicas mortas não causam falso trabalho pendente.

### Vida, segurança e erros

- Pai descarta filho ainda presente na fila.
- Effect descarta seu owner durante callback, inclusive com reutilização do
  slot antes de retornar: nenhuma restauração de closure ou commit no slot novo.
- Source nova descartada após leitura: não publicar aresta morta, reinvalidar
  sobrevivente; reexecução pode abandonar o handle.
- Disposal idempotente, cleanup exatamente uma vez e ordem definida.
- Slot reutilizado rejeita handle antigo; handles de runtime diferente são rejeitados.
- Source descartada invalida dependente sobrevivente, sem valor antigo confiável.
- Ciclo direto/indireto e ciclo na travessia de `Check`.
- Loop de writes limitado por rodadas, sem stack overflow.
- Profundidade excessiva retorna erro definido ou usa caminho iterativo.
- Escrita em memo/comparador é rejeitada.
- Criação/disposal/cleanup em memo e acesso ao grafo pelo comparador rejeitados.
- `untrack`, batch e outro runtime não removem restrições de pureza.
- Read rastreado de outro runtime rejeitado; leitura explícita sob untrack não
  cria dependência cruzada.
- Update com write reentrante, disposal e panic em `Clone`: conflito/erro sem
  sobrescrita, retry automático ou borrow interno durante código do usuário.
- Esgotamento de identidade, geração, revisão, sequência de criação e rodada
  simulado com contadores pequenos: erro/aposentadoria sem wrap.
- Panic em compute/comparator/cleanup preserva invariantes ou envenena o runtime
  de forma explícita; nunca retoma com valor derivado falsamente `Clean`.
- Valores type-erased e closures descartados sem aliasing e sem leaks lógicos.
- Falha em cleanup seguida por outros cleanups: todos tentados uma vez, erro
  original preservado, writes pós-poison rejeitados e teardown concluído.
- Panic em hook/destrutor: política de poison e unwind verificada no binário
  correto; não presumir execução de guards em `panic=abort`.
- Snapshot já entregue sobrevive ao disposal; APIs reativas pós-poison falham,
  mas disposal/shutdown continuam disponíveis.

### Propriedades e desempenho

- Testes aleatórios de reads/writes/branching/disposal comparados com modelo simples.
- Verificador das arestas bidirecionais e filas em builds de teste.
- Benchmarks de chains, diamonds, fan-out alto e dependências condicionais.
- Separar custo de marcação, verificação, execução, reconciliação e disposal.
- Contar allocations e memória retida depois do aquecimento e do descarte.
- Medir separadamente leitura emprestada, cópia, clone de `T`, clone de `Rc<T>`
  explícito, construção dos slots reutilizáveis e publicação de resultado de
  memo, sem atribuir custo de valor ao scheduler.
- Medir baixo/alto fan-in e readers aninhados para escolher o limiar de promoção
  da deduplicação; comparar layouts hot/cold com baseline seguro.
- Contar rodadas/tickets além de callbacks: não aceitar rounds vazios ou
  reexecuções por invalidação já consumida como custo inevitável.
- Confirmar que histórico de handles mortos não aumenta indefinidamente o custo.
- Se introduzir `unsafe`, adicionar validação apropriada com Miri e revisar
  explicitamente as invariantes de aliasing/lifetime.

## 15. Ordem recomendada de implementação

1. Runtime explícito, handles geracionais, owners mínimos e guards de contexto/
   capacidade; política de poison e teardown estrutural desde o início.
2. Signals, leituras tipadas explícitas, comparadores fora de borrows e revisões; testes
   de erro, acesso obsoleto, clone/update e overflow.
3. Memos puros, tracking empilhado, atualização lazy, dependências dinâmicas e
   equality cut; revisão de valor consumida por aresta.
4. Effects, transições de duas filas, conclusão por revisões de leitura e flush
   limitado; testes de reinvalidação e rodadas sem trabalho redundante.
5. Owners de execução reutilizáveis, disposal reentrante, cleanup e unwind;
   validar self-disposal antes de integrar qualquer host.
6. Batch/untrack e protocolo de notificações desacoplado.
7. Testes de propriedades e benchmarks para estabelecer baseline, além dos
   testes unitários e verificadores incrementais desde cada etapa anterior.
8. Adaptador experimental ao host GPUI, com scheduler gráfico independente.
9. Otimizações medidas de layout de memória e travessias iterativas.

Critério de saída do núcleo: contratos e invariantes testados sem janela,
nenhuma dependência de GUI/GPU e sem gaps conhecidos de reinvalidação,
tracking aninhado ou disposal. Integração gráfica não deve servir para esconder
falhas do algoritmo com redraw contínuo.
