# Arquitetura de acesso a handles

## Referência e desenho selecionado

Sycamore 0.9.4 também usa handles não proprietários e generacionais. Seu
NodeId é uma chave compacta do SlotMap (índice/version de 32 bits); o handle
carrega também uma referência ao Root. A resolução checa bounds/version,
usa unsafe para acessar o valor da union e downcast de Any para o tipo.
SlotMap documenta recorrência possível após 2^31 reutilizações do mesmo slot.

Nosso desenho preserva Runtime explícito, identidade única u64, erros
fallible, disposal, capabilities, poison e revalidação após user code.
O plano implementado é:

1. **IDs compactos:** SlotId contém índice u32 e geração NonZeroU32: 8 bytes,
   também para Option<SlotId>. HandleId tem 16 bytes. Sem ponteiros persistentes,
   sem prolongar vida dos nós e sem Send/Sync.
2. **Sem wrap:** geração esgotada aposenta o slot. Índice u32::MAX é reservado;
   cada arena admite até u32::MAX slots físicos. Preflight de capacidade antes
   de consumir payload retorna CounterExhausted; recursos rejeitados são
   liberados fora do borrow do grafo. Diagnósticos convertem para usize/u64.
3. **Free list compacta:** Option<NonZeroU32> codifica índice + 1; nenhum slot
   aposentado volta à lista livre.
4. **Copy direto:** resolver, validar tipo, copiar e registrar revisões no
   mesmo borrow, sem Rc temporário. Dirty memos encerram o borrow antes de
   avaliar e resolvem novamente depois. Clone/with conservam a célula própria
   e seus guards; nenhum borrow do grafo atravessa código do usuário.
5. **Estado quente:** RuntimeStatus em Cell, separado do payload frio de erro.
   Healthy → Closed ou Poisoned; Closed → Poisoned; Poisoned nunca volta.
   O verifier opcional audita coerência status/erro sem custo no build normal.
6. **Unsafe restrito:** get/get_mut da arena eliminam apenas o teste redundante
   de Option<T>, após bounds, geração e vida. Não há índice unchecked, cast de
   tipo unchecked ou ponteiro guardado no handle.

Não é uma promessa de ótimo global. Scheduling, equality cut e ascensão
recursiva permanecem; DFS e migração hot/cold de nós não foram implementados.

## Invariantes do unsafe

Somente arena.rs altera os campos privados: Alive/Disposing contém Some(T),
Vacant/Retired contém None. Insert instala valor e vida antes de devolver ID;
renew só altera geração; mark_disposing conserva T mas impede acesso normal;
remove move T e muda vida/geração sem executar callback ou destructor entre
essas operações. O caller libera o recurso fora do borrow.

Bounds/geração/Alive provam Some antes de unwrap_unchecked. A referência tem
vida ligada a &self ou &mut self; empréstimos Rust/RefCell impedem realocação
e alias mutável enquanto usada. Nunca reutilizar referência ou ponteiro após
compute, comparator, cleanup, Clone, callback ou destructor: resolver de novo.
Option<T> continua responsável pela destruição normal; não adotamos union
manual nem storage instável acessado por ponteiros persistentes.

## Validação

Modelo de transições audita inicialização, contagem, leitura/mutação, renewal,
disposal e handles antigos. Testes cobrem geração zero, últimas gerações,
aposentadoria, índices inválidos, capacidade simulada e destructor guard em
payload rejeitado. Miri passou 10 testes da arena e 6 de leitura; o modelo
executa 3 seeds, 256 passos nativos ou 64 no interpretador. Isso não substitui
uma prova formal e não representa toda a suíte no Miri.

Medições e limitações: [HANDLE-ARCHITECTURE.md](../benches/.log/HANDLE-ARCHITECTURE.md).
