//! O teto de reconhecimentos (`MAX_ACKS` = 256, `foreign.rs`) contra um armazenamento hostil
//! (P-49, Sessão 131, ADR 0018 "Ainda não corrigido, por escolha"). Pergunta: um `Engine` ligado
//! **direto** a um backend que lista mais de 256 manifests de época mais nova, que ele não abre,
//! fica travado? O `apply` agora chama `load()` sempre (arca#211), então ele entra na pergunta
//! junto com `heads`, `merge` e `snapshot`, e o `begin_epoch` também (ele chama `load()` com as
//! chaves novas e, **só na primeira tentativa**, de novo com as velhas; ver abaixo).
//!
//! Por que o teto é alcançável: `load()` lê **todos** os manifests do armazenamento do `Engine`
//! (`scan_manifests`) e, para cada um de época maior que a das chaves, `unacknowledged` só o
//! descarta se o par `(época, id)` está nos reconhecidos. `ForeignAcks::add` guarda no máximo 256
//! pares e, ao passar disso, tira o mais antigo (descarte FIFO). Com 300 manifests estrangeiros,
//! reconhecer os 300 deixa 256 e **desreconhece 44**, que bloqueiam de novo; reconhecer esses 44
//! tira outros 44 (os mais antigos). Nada no `Engine` apaga objeto do armazenamento, então a saída
//! não existe pela API do núcleo. (O `pull` não grava manifest estrangeiro na réplica; este teste
//! é o caso do `Engine` **sem** réplica local, o limite que o ADR 0018 já registra em "Não
//! validado". O caminho com réplica e `pull` não é testado aqui.)
//!
//! "Travado" vale só para as operações que chamam `load()` (conferido por `grep` em `engine.rs` e
//! `engine/verify.rs`): `heads`, `verify` (`verify.rs`), `conflicts_between`, `merge` e
//! `merge_with_losers` (ambos por `merge_inner`), `snapshot` e `snapshot_allowing_empty`, `apply` e
//! `begin_epoch`. `foreign_epochs`, `acknowledge_foreign`, `restore` e `pull` não chamam `load()` e
//! seguem funcionando (o teste de 300 confere os quatro; `verify`, `merge_with_losers`,
//! `snapshot_allowing_empty` e `conflicts_between` não são chamados aqui: eram por leitura e foram
//! medidas no arca#222, que veta com 257 e destrava com 256).
//!
//! O `begin_epoch` tem dois `load()`: o `next.load()` (chaves novas), que roda sempre, e o
//! `self.load()` (chaves velhas), que só roda na **primeira tentativa** (`own.is_empty()`, sem
//! manifest da época nova). Na **retomada** só o `next.load()` roda, e o veto de >256 forjados de
//! época 2 (que vem do `self.load()`) não se aplica. **Nenhum teste aqui fixa a retomada**; ela foi
//! medida no arca#222 (passa com 257 e com 1 forjado de época 2) e, com um pendente de época 3,
//! está em `tests/begin_epoch_inherits_acks.rs`.
//!
//! Cenário: o dispositivo `a` (época 1, chave velha) publica um manifest legítimo com um arquivo
//! de Vault. Um atacante grava no **mesmo** armazenamento N manifests de uma época maior, cifrados
//! com uma chave que `a` não tem (ele também não precisa da chave certa: o cabeçalho em claro
//! basta, ADR 0018). Cada teste afirma antes a **premissa** (o backend lista 1 + N manifests; os N
//! têm id distinto, o cabeçalho da época forjada e não abrem nem com as chaves de `a` nem com as
//! da época 2; `a` os vê todos como estrangeiros não reconhecidos, o que prova que o `Engine`
//! chegou ao caminho do teto) e só depois o **resultado**.
//!
//! Resposta esperada, que o CI confirma:
//! - Com mais de 256 forjados, `heads`, `merge`, `snapshot` e `apply` dão `NeedsNewKey { have: 1,
//!   found: 2 }` depois de qualquer número de reconhecimentos, com e sem `with_state` (e depois de
//!   reiniciar o `Engine`). O `begin_epoch` também, na primeira tentativa: ele chama `load()` com as
//!   chaves **velhas** (`engine.rs`, depois do `next.load()` com as novas), e um forjado de época 2
//!   é "época mais nova" para elas. A rotação, que é a remediação do T8, fica vetada.
//! - Com exatamente 256, um reconhecimento basta, e o `begin_epoch` publica a época 2; os forjados
//!   de época 2 passam a ser `rejected` do `Engine` novo (mesma época, não abrem), então o
//!   `snapshot` seguinte dá `Diverged`.
//! - Com 257, sobra exatamente um não reconhecido até alguém **apagar do backend** (fora da API do
//!   `Engine`) um dos objetos.
//! - Achado à parte (não é o teto), **corrigido na rodada 9**: sem `with_state`, o `Engine` novo do
//!   `begin_epoch` nascia sem os reconhecimentos do velho, e um **único** forjado de época 3 vetava
//!   a rotação mesmo reconhecido. Agora o `Engine` novo herda tudo do velho (reconhecidos e
//!   pendentes) e a rotação passa, com e sem estado; o id novo de época 3 continua vetando (ver
//!   `tests/begin_epoch_inherits_acks.rs`).
//!
//! Isto é caracterização do comportamento atual, não uma correção, no que toca ao teto: os testes
//! `..._stay_blocked_...`, `..._257_...` e `..._veto_begin_epoch` ficam vermelhos no dia em que o
//! teto deixar de travar (por exemplo, `unacknowledged` passar a contar só o que cabe, ou o
//! reconhecimento ganhar outra forma), e é esse o sinal para atualizar o ADR 0018. O teste
//! `..._is_inherited_and_lets_begin_epoch_through` (época 3, sem estado) não é do teto: ele fixa a
//! correção do achado à parte e fica vermelho se a herança dos reconhecimentos sumir.

mod common;

use arca_core::backend::Backend;
use arca_core::disk::DiskStore;
use arca_core::engine::{Applied, Engine, EngineError, ForeignEpoch, Restored, Roots, Snapshot};
use arca_core::manifest::{Hash32, Manifest};
use arca_core::state::DeviceState;
use arca_core::store::BlobStore;
use arca_core::{EpochKeys, Header, ObjectId, ObjectKind, VaultKey};
use common::{flood, old_keys, Tmp};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// O valor de `MAX_ACKS` (`foreign.rs`, privado ao crate). Os testes de fronteira (256 destrava,
/// 257 trava) são o que prova que é este o valor: se mudar, eles falham.
const ACK_CAP: usize = 256;

/// A época que o atacante grava na maioria dos testes: maior que a do dispositivo (1) e igual à
/// que a rotação vai criar.
const FOREIGN_EPOCH: u32 = 2;

/// A chave que a rotação usa (`begin_epoch`).
fn next_vault_key() -> VaultKey {
    VaultKey::from_bytes([4u8; 32])
}

fn next_keys() -> EpochKeys {
    EpochKeys::derive(&next_vault_key(), 2)
}

/// O erro que bloqueia o dispositivo da época 1 quando o armazenamento mostra a época 2.
fn blocked() -> EngineError {
    EngineError::NeedsNewKey {
        have: 1,
        found: FOREIGN_EPOCH,
    }
}

struct Lab {
    tmp: Tmp,
    /// O armazenamento de `a`: ele **é** o backend hostil (sem réplica local).
    store: Backend,
    roots: Roots,
    /// O manifest legítimo de `a`, da época 1.
    own: ObjectId,
    with_state: bool,
}

impl Lab {
    fn engine(&self) -> Engine<Backend> {
        let engine = Engine::new(self.store.clone(), old_keys(), "a").unwrap();
        if self.with_state {
            engine.with_state(self.state_path())
        } else {
            engine
        }
    }

    fn state_path(&self) -> PathBuf {
        self.tmp.0.join("a-state")
    }

    async fn manifest_count(&self) -> usize {
        self.store.list(ObjectKind::Manifest).await.unwrap().len()
    }

    async fn blob_ids(&self) -> HashSet<[u8; 32]> {
        let blobs = self.store.list(ObjectKind::Blob).await.unwrap();
        blobs.iter().map(|id| id.hash).collect()
    }
}

/// `a` publica o manifest legítimo da época 1 (um arquivo no Vault) e o controle prova que, antes
/// da inundação, nada bloqueia: uma ponta só, nenhum estrangeiro, `apply` aplica (já igual).
async fn setup(with_state: bool) -> (Lab, Engine<Backend>) {
    let tmp = Tmp::new();
    let store = Backend::Disk(DiskStore::open(tmp.0.join("store")).unwrap());
    let vault = tmp.0.join("a-vault");
    fs::create_dir_all(vault.join("notas")).unwrap();
    fs::write(
        vault.join("notas/diario.md"),
        b"diario de a: 27 de novembro",
    )
    .unwrap();
    let roots = Roots {
        vault: Some(vault),
        config: None,
        secret: None,
    };
    // `own` é preenchido logo abaixo, pelo id do manifest que o primeiro `snapshot` publica
    let mut lab = Lab {
        tmp,
        store,
        roots,
        own: ObjectId::of(ObjectKind::Manifest, b""),
        with_state,
    };
    let engine = lab.engine();
    let Snapshot::Created { id, .. } = engine.snapshot(&lab.roots, 1).await.unwrap() else {
        panic!("esperava Created");
    };
    lab.own = id;

    // controle: sem inundação, nada bloqueia
    assert_eq!(
        lab.store.list(ObjectKind::Manifest).await.unwrap(),
        vec![id]
    );
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    let heads = engine.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, id);
    assert_eq!(
        engine.apply(None, &id, &lab.roots).await,
        Ok(Applied {
            created: 0,
            updated: 0,
            removed: 0,
            unchanged: 1
        })
    );
    (lab, engine)
}

/// A premissa: o backend lista o manifest de `a` e os `flood.len()` forjados; os forjados têm id
/// distinto, cabeçalho da `epoch` e **não abrem** nem com as chaves de `a` nem com as da época 2;
/// e `a` os enxerga todos como estrangeiros não reconhecidos (é o que leva `load()` até o teto).
async fn assert_the_engine_reaches_the_cap_path(
    lab: &Lab,
    engine: &Engine<Backend>,
    flood: &[ObjectId],
    epoch: u32,
) {
    let listed = lab.store.list(ObjectKind::Manifest).await.unwrap();
    assert_eq!(listed.len(), flood.len() + 1);
    assert!(listed.contains(&lab.own));
    let distinct: HashSet<[u8; 32]> = flood.iter().map(|id| id.hash).collect();
    assert_eq!(distinct.len(), flood.len());
    assert!(!distinct.contains(&lab.own.hash));
    for id in flood {
        assert!(listed.contains(id));
        let bytes = lab.store.get(id).await.unwrap().unwrap();
        let (header, _) = Header::decode(&bytes).unwrap();
        assert_eq!(header.key_epoch, epoch);
        assert!(Manifest::open(&old_keys(), &bytes).is_err());
        assert!(Manifest::open(&next_keys(), &bytes).is_err());
    }
    let seen = engine.foreign_epochs().await.unwrap();
    assert_eq!(seen.len(), flood.len());
    assert!(seen.iter().all(|f| distinct.contains(&f.id.hash)));
    assert!(seen.iter().all(|f| f.epoch == epoch));
}

/// `heads`, `merge`, `snapshot` e `apply` recusam, e nada é escrito.
async fn assert_blocked(lab: &Lab, engine: &Engine<Backend>) {
    assert_eq!(engine.heads().await.map(|_| ()), Err(blocked()));
    assert_eq!(engine.merge(20).await.map(|_| ()), Err(blocked()));
    assert_eq!(
        engine.snapshot(&lab.roots, 21).await.map(|_| ()),
        Err(blocked())
    );
    assert_eq!(
        engine.apply(None, &lab.own, &lab.roots).await.map(|_| ()),
        Err(blocked())
    );
}

/// O que sobra depois de `acknowledge_foreign` de tudo que `foreign_epochs` mostrava.
async fn acknowledge_everything(engine: &Engine<Backend>) -> Vec<ForeignEpoch> {
    let shown = engine.foreign_epochs().await.unwrap();
    engine.acknowledge_foreign(&shown).await.unwrap();
    engine.foreign_epochs().await.unwrap()
}

fn hashes(list: &[ForeignEpoch]) -> HashSet<[u8; 32]> {
    list.iter().map(|f| f.id.hash).collect()
}

/// O estado de `a` não andou (nada foi aceito além do manifest legítimo).
fn assert_state_untouched(lab: &Lab) {
    if lab.with_state {
        assert_eq!(
            DeviceState::load(&lab.state_path()).unwrap(),
            Some(DeviceState {
                key_epoch: 1,
                last_accepted: Hash32(lab.own.hash),
            })
        );
    }
}

/// 300 > 256: reconhecer não destrava, em nenhuma rodada, em lotes ou de uma vez, e (com estado) nem
/// depois de reiniciar o `Engine`. O que não chama `load()` segue funcionando.
async fn above_the_cap_the_engine_stays_blocked(with_state: bool) {
    const FLOOD: usize = 300;
    const _: () = assert!(FLOOD > ACK_CAP);
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, FLOOD, FOREIGN_EPOCH).await;
    let forged_hashes: HashSet<[u8; 32]> = forged.iter().map(|id| id.hash).collect();
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged, FOREIGN_EPOCH).await;
    assert_blocked(&lab, &engine).await;

    // rodada 1: reconhecer tudo, em lotes de 50 (o lote não muda o teto)
    for chunk in engine.foreign_epochs().await.unwrap().chunks(50) {
        engine.acknowledge_foreign(chunk).await.unwrap();
    }
    let mut previous = engine.foreign_epochs().await.unwrap();
    assert_eq!(previous.len(), FLOOD - ACK_CAP);
    assert!(hashes(&previous).is_subset(&forged_hashes));
    assert_blocked(&lab, &engine).await;

    // rodadas 2 e 3: reconhecer o que sobrou desreconhece outros tantos, e **outros**: o que sai é
    // o mais antigo (FIFO), então o conjunto que sobra não repete o da rodada anterior
    for _ in 0..2 {
        let left = acknowledge_everything(&engine).await;
        assert_eq!(left.len(), FLOOD - ACK_CAP);
        assert!(hashes(&left).is_subset(&forged_hashes));
        assert!(hashes(&left).is_disjoint(&hashes(&previous)));
        assert_blocked(&lab, &engine).await;
        previous = left;
    }

    if with_state {
        // reiniciar o app: engine novo, mesmo armazenamento e mesmo arquivo de estado. Os 256
        // reconhecidos estão em disco (`<estado>.foreign`) e os outros 44 continuam bloqueando
        let restarted = lab.engine();
        assert_eq!(
            hashes(&restarted.foreign_epochs().await.unwrap()),
            hashes(&previous)
        );
        assert_blocked(&lab, &restarted).await;
        let left = acknowledge_everything(&restarted).await;
        assert_eq!(left.len(), FLOOD - ACK_CAP);
        assert_blocked(&lab, &restarted).await;
    }

    // o que não chama `load()` segue funcionando: `foreign_epochs` e `acknowledge_foreign` (acima),
    // `restore` e `pull`
    let restored = Roots {
        vault: Some(lab.tmp.0.join("restored")),
        config: None,
        secret: None,
    };
    assert_eq!(
        engine.restore(&lab.own, &restored).await,
        Ok(Restored { files: 1 })
    );
    assert_eq!(
        fs::read(restored.vault.as_ref().unwrap().join("notas/diario.md")).unwrap(),
        b"diario de a: 27 de novembro"
    );
    let remote = Backend::Disk(DiskStore::open(lab.tmp.0.join("remote")).unwrap());
    let report = engine.pull(&remote).await.unwrap();
    assert_eq!(report.copied, 0);
    assert!(report.foreign_epochs.is_empty(), "{report:?}");
    assert_blocked(&lab, &engine).await;

    // nada foi escrito no armazenamento por nada disto
    assert_eq!(lab.manifest_count().await, FLOOD + 1);
    assert_eq!(lab.blob_ids().await.len(), 1);
    assert_state_untouched(&lab);
}

#[tokio::test]
async fn without_state_more_than_256_foreign_manifests_stay_blocked_whatever_is_acknowledged() {
    above_the_cap_the_engine_stays_blocked(false).await;
}

#[tokio::test]
async fn with_state_more_than_256_foreign_manifests_stay_blocked_even_after_a_restart() {
    above_the_cap_the_engine_stays_blocked(true).await;
}

/// Controle da fronteira: com exatamente 256, um reconhecimento basta e tudo volta a funcionar
/// (com estado, também depois de reiniciar o `Engine`). Sem isto, o teste de cima poderia estar
/// vermelho por outro motivo que não o teto.
async fn at_the_cap_one_acknowledgement_unblocks(with_state: bool) {
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, ACK_CAP, FOREIGN_EPOCH).await;
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged, FOREIGN_EPOCH).await;
    assert_blocked(&lab, &engine).await;

    assert!(acknowledge_everything(&engine).await.is_empty());
    let heads = engine.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, lab.own);
    // os forjados de época maior reconhecidos não aparecem como ponta nem como rejeitados
    assert!(heads.rejected.is_empty() && heads.superseded.is_empty());
    assert_eq!(
        engine.apply(None, &lab.own, &lab.roots).await,
        Ok(Applied {
            created: 0,
            updated: 0,
            removed: 0,
            unchanged: 1
        })
    );
    match engine.snapshot(&lab.roots, 22).await.unwrap() {
        Snapshot::Unchanged { head, .. } => assert_eq!(head, lab.own),
        other => panic!("esperava Unchanged, veio {other:?}"),
    }
    if with_state {
        // o reconhecimento está em disco: um `Engine` novo também está livre
        let restarted = lab.engine();
        assert!(restarted.foreign_epochs().await.unwrap().is_empty());
        let heads = restarted.heads().await.unwrap();
        assert_eq!(heads.tips.len(), 1);
        assert_eq!(heads.tips[0].id, lab.own);
    }
    assert_state_untouched(&lab);
}

#[tokio::test]
async fn without_state_exactly_256_foreign_manifests_are_unblocked_by_one_acknowledgement() {
    at_the_cap_one_acknowledgement_unblocks(false).await;
}

#[tokio::test]
async fn with_state_exactly_256_foreign_manifests_are_unblocked_by_one_acknowledgement() {
    at_the_cap_one_acknowledgement_unblocks(true).await;
}

/// 257: sobra **exatamente um**, sempre, e só apagar um objeto do backend (o dono do backend, fora
/// da API do `Engine`) destrava. Isola a causa: o bloqueio vem da contagem de objetos estrangeiros
/// no armazenamento, não do reconhecimento nem do estado.
async fn one_past_the_cap_leaves_exactly_one_until_the_backend_drops_it(with_state: bool) {
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, ACK_CAP + 1, FOREIGN_EPOCH).await;
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged, FOREIGN_EPOCH).await;
    assert_blocked(&lab, &engine).await;

    let mut left = Vec::new();
    for _ in 0..3 {
        left = acknowledge_everything(&engine).await;
        assert_eq!(left.len(), 1, "{left:?}");
        assert_blocked(&lab, &engine).await;
    }

    if with_state {
        // reiniciar o app: o que sobrou é o mesmo, lido do arquivo, e continua bloqueando
        let restarted = lab.engine();
        assert_eq!(restarted.foreign_epochs().await.unwrap(), left);
        assert_blocked(&lab, &restarted).await;
    }

    // o backend apaga o que sobrou: os 256 que restam já estão todos reconhecidos
    lab.store.delete(&left[0].id).await.unwrap();
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    let heads = engine.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, lab.own);
    assert_eq!(
        engine.apply(None, &lab.own, &lab.roots).await,
        Ok(Applied {
            created: 0,
            updated: 0,
            removed: 0,
            unchanged: 1
        })
    );
    assert_state_untouched(&lab);
}

#[tokio::test]
async fn without_state_257_foreign_manifests_leave_one_blocking_until_the_backend_drops_it() {
    one_past_the_cap_leaves_exactly_one_until_the_backend_drops_it(false).await;
}

#[tokio::test]
async fn with_state_257_foreign_manifests_leave_one_blocking_until_the_backend_drops_it() {
    one_past_the_cap_leaves_exactly_one_until_the_backend_drops_it(true).await;
}

/// A rotação com 300 forjados **da época que ela vai criar** (a próxima época é previsível, ADR
/// 0018 item 3). O `next.load()` do `begin_epoch` (chaves novas) passa, porque para elas o forjado é
/// "da minha época, ilegível" (`rejected`). O `self.load()` (chaves velhas) é o que veta: para
/// elas o forjado é "época mais nova não reconhecida", e com mais de 256 sempre sobra algum. Esse
/// 2º `load()` só roda na primeira tentativa (sem manifest da época nova), que é o caso aqui; a
/// retomada não é testada.
async fn above_the_cap_begin_epoch_is_vetoed(with_state: bool) {
    const FLOOD: usize = 300;
    const _: () = assert!(FLOOD > ACK_CAP);
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, FLOOD, FOREIGN_EPOCH).await;
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged, FOREIGN_EPOCH).await;
    let blobs = lab.blob_ids().await;
    assert_eq!(blobs.len(), 1);

    // sem reconhecer, e depois de reconhecer tudo (sobram 44): o mesmo veto
    for round in 0..2 {
        if round == 1 {
            assert_eq!(acknowledge_everything(&engine).await.len(), FLOOD - ACK_CAP);
        }
        let outcome = engine
            .begin_epoch(&next_vault_key(), &lab.roots, 30)
            .await
            .map(|_| ());
        assert_eq!(outcome, Err(blocked()), "rodada {round}");
    }

    // nada foi publicado: nenhum manifest da época 2, nenhum blob novo, estado onde estava
    assert_eq!(lab.manifest_count().await, FLOOD + 1);
    assert_eq!(lab.blob_ids().await, blobs);
    assert_state_untouched(&lab);
}

#[tokio::test]
async fn without_state_more_than_256_forged_manifests_of_the_next_epoch_veto_begin_epoch() {
    above_the_cap_begin_epoch_is_vetoed(false).await;
}

#[tokio::test]
async fn with_state_more_than_256_forged_manifests_of_the_next_epoch_veto_begin_epoch() {
    above_the_cap_begin_epoch_is_vetoed(true).await;
}

/// Controle: com exatamente 256 forjados de época 2, o `begin_epoch` é vetado até o reconhecimento
/// e passa depois dele. A rotação publica a época 2 e os forjados ficam como `rejected` do `Engine`
/// novo (mesma época, não abrem): o `snapshot` seguinte dá `Diverged`.
async fn at_the_cap_begin_epoch_goes_through_after_one_acknowledgement(with_state: bool) {
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, ACK_CAP, FOREIGN_EPOCH).await;
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged, FOREIGN_EPOCH).await;
    let outcome = engine
        .begin_epoch(&next_vault_key(), &lab.roots, 30)
        .await
        .map(|_| ());
    assert_eq!(outcome, Err(blocked()));
    assert_eq!(lab.manifest_count().await, ACK_CAP + 1);

    assert!(acknowledge_everything(&engine).await.is_empty());
    let (next, snapshot) = engine
        .begin_epoch(&next_vault_key(), &lab.roots, 31)
        .await
        .unwrap();
    let Snapshot::Created {
        id,
        manifest,
        new_blobs,
        ..
    } = snapshot
    else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    assert_eq!(manifest.key_epoch, FOREIGN_EPOCH);
    assert_eq!(manifest.base_version, None);
    assert_eq!(manifest.counter, 1);
    assert_eq!(manifest.files.len(), 1);
    assert_eq!(new_blobs, 1);
    assert_eq!(lab.manifest_count().await, ACK_CAP + 2);
    if with_state {
        assert_eq!(
            DeviceState::load(&lab.state_path()).unwrap(),
            Some(DeviceState {
                key_epoch: FOREIGN_EPOCH,
                last_accepted: Hash32(id.hash),
            })
        );
    }

    // para o `Engine` novo os 256 forjados são da época dele e não abrem: `rejected`, nunca ponta
    let heads = next.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, id);
    assert_eq!(heads.rejected.len(), ACK_CAP);
    assert_eq!(
        next.snapshot(&lab.roots, 32).await.map(|_| ()),
        Err(EngineError::Diverged {
            tips: 1,
            rejected: ACK_CAP
        })
    );
}

#[tokio::test]
async fn without_state_with_256_forged_manifests_of_the_next_epoch_begin_epoch_goes_through_once_acknowledged(
) {
    at_the_cap_begin_epoch_goes_through_after_one_acknowledgement(false).await;
}

#[tokio::test]
async fn with_state_with_256_forged_manifests_of_the_next_epoch_begin_epoch_goes_through_once_acknowledged(
) {
    at_the_cap_begin_epoch_goes_through_after_one_acknowledgement(true).await;
}

/// Achado à parte, sem relação com o teto: **um** forjado de época 3 (uma acima da que a rotação
/// cria), reconhecido. O `next.load()` do `begin_epoch` usa um `Engine` novo, e o que ele sabe dos
/// reconhecimentos vem do arquivo de estado, se houver, ou da memória do `Engine` novo, que herda
/// tudo do velho, reconhecidos e pendentes (antes nascia vazia e o reconhecimento não chegava lá).
async fn one_forged_manifest_of_a_later_epoch(with_state: bool) -> (Lab, Engine<Backend>) {
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, 1, 3).await;
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged, 3).await;
    // a pessoa vê e reconhece o objeto: o `Engine` velho volta a operar
    assert!(acknowledge_everything(&engine).await.is_empty());
    let heads = engine.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, lab.own);
    (lab, engine)
}

#[tokio::test]
async fn without_state_one_acknowledged_forged_manifest_of_epoch_3_is_inherited_and_lets_begin_epoch_through(
) {
    let (lab, engine) = one_forged_manifest_of_a_later_epoch(false).await;
    // premissa: o único forjado já foi reconhecido no `Engine` velho (nada a reconhecer de novo,
    // conferido por `one_forged_manifest_of_a_later_epoch`)
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    assert_eq!(lab.manifest_count().await, 2);
    assert_eq!(lab.blob_ids().await.len(), 1);

    // o `Engine` novo do `begin_epoch` herda o reconhecimento (antes: `NeedsNewKey { have: 2,
    // found: 3 }`), então o forjado de época 3 não veta a rotação
    let outcome = engine.begin_epoch(&next_vault_key(), &lab.roots, 30).await;
    assert_eq!(outcome.as_ref().err(), None, "a rotação devia passar");
    let (next, snapshot) = outcome.unwrap();
    let Snapshot::Created {
        id,
        manifest,
        new_blobs,
        ..
    } = snapshot
    else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    assert_eq!(manifest.key_epoch, FOREIGN_EPOCH);
    assert_eq!(new_blobs, 1);
    // o manifest novo entrou e o blob do arquivo foi recifrado com a chave nova
    assert_eq!(lab.manifest_count().await, 3);
    assert_eq!(lab.blob_ids().await.len(), 2);
    // o forjado reconhecido não vira ponta nem rejeitado para o `Engine` novo, e nada bloqueia
    let heads = next.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, id);
    assert!(heads.rejected.is_empty());
    // o reconhecimento chegou ao `Engine` novo: nada a reconhecer (só lê, não muda o `next`)
    assert!(next.foreign_epochs().await.unwrap().is_empty());
}

#[tokio::test]
async fn with_state_one_acknowledged_forged_manifest_of_epoch_3_lets_begin_epoch_through() {
    let (lab, engine) = one_forged_manifest_of_a_later_epoch(true).await;
    let (next, snapshot) = engine
        .begin_epoch(&next_vault_key(), &lab.roots, 30)
        .await
        .unwrap();
    let Snapshot::Created { id, manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    assert_eq!(manifest.key_epoch, FOREIGN_EPOCH);
    let heads = next.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, id);
    assert!(heads.rejected.is_empty());
}
