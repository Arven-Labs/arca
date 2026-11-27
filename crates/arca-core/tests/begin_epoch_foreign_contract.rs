//! O que o `Engine::begin_epoch` faz com manifests de **época igual à que ele cria** que não abrem
//! com a chave nova (ADR 0018, itens 3 e 5, e "Ainda não corrigido, por escolha"). Isto é só
//! prova: nenhuma correção. A pergunta do dono é se o `begin_epoch` poderia ignorar o objeto de
//! época igual à que cria **sem perder** a proteção contra outro dispositivo legítimo que já
//! rotacionou; os testes (a) e (b) fixam o que uma correção futura teria de manter e o que ela
//! mudaria.
//!
//! O cabeçalho de um manifest de época nova **não é autenticado** (só o corpo é, pela chave): para
//! quem não tem a chave, "rotação feita por outro dispositivo com outra chave" e "manifest forjado
//! de época 2" são o mesmo objeto. O `begin_epoch`, na **primeira tentativa** (`next.load()` das
//! chaves novas não acha manifest dele, `own` vazio), faz `self.load()` com as chaves **velhas**, e
//! para elas qualquer objeto de época 2 não reconhecido é "época mais nova": `NeedsNewKey`.
//!
//! Cenário: o dispositivo `a` (época 1, chave velha) publicou um manifest legítimo (um arquivo de
//! Vault). Cada teste afirma antes a **premissa** (o que o armazenamento tem, com que chave cada
//! objeto abre, e por qual caminho o `begin_epoch` passa) e só depois o **resultado**.
//!
//! Resposta esperada, que o CI confirma (hipótese até lá):
//! - (a) `legit_...` e `forged_...`: `begin_epoch(K2)` na 1ª tentativa dá `NeedsNewKey { have: 1,
//!   found: 2 }` e nada é escrito, tanto para a rotação legítima de outro dispositivo (chave K2'
//!   diferente da K2) quanto para um forjado: o veto não distingue os dois, e é isso que o
//!   contrato exige. Uma correção que ignore época igual à que cria faria **os dois** passarem
//!   (publicariam um 2º manifest de época 2, com K2, ao lado do de K2'): ficam vermelhos.
//! - (b) Retomada com 257 forjados de época 2 **e** um manifest de época 2 que abre com a chave
//!   nova: passa (`Unchanged`), porque só o `next.load()` roda. O controle (mesmos 257 forjados,
//!   sem o manifest da tentativa anterior) dá `NeedsNewKey`.
//! - (d) Depois de uma rotação bem-sucedida com **um** forjado de época 2 no armazenamento (que
//!   foi reconhecido para a rotação passar), o `snapshot` do `Engine` novo dá `Diverged { tips: 1,
//!   rejected: 1 }`.

use arca_core::backend::Backend;
use arca_core::disk::DiskStore;
use arca_core::engine::{Engine, EngineError, ForeignEpoch, Roots, Snapshot};
use arca_core::manifest::{Hash32, Manifest};
use arca_core::state::DeviceState;
use arca_core::store::BlobStore;
use arca_core::{EpochKeys, Header, ObjectId, ObjectKind, VaultKey};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

/// A época que a rotação cria e a que o forjador e o outro dispositivo gravam.
const NEXT_EPOCH: u32 = 2;

/// Pasta única em `temp_dir()`, removida no `Drop` (o `testutil` do crate não é visível aqui).
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Tmp {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("arca-it-{hex}"));
        fs::create_dir_all(&path).unwrap();
        Tmp(path)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn old_keys() -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([3u8; 32]), 1)
}

/// K2: a chave que **este** dispositivo passa ao `begin_epoch`.
fn k2() -> VaultKey {
    VaultKey::from_bytes([4u8; 32])
}

fn k2_keys() -> EpochKeys {
    EpochKeys::derive(&k2(), NEXT_EPOCH)
}

/// K2': a chave que **outro** dispositivo legítimo usou na rotação dele.
fn other_vault_key() -> VaultKey {
    VaultKey::from_bytes([5u8; 32])
}

fn other_keys() -> EpochKeys {
    EpochKeys::derive(&other_vault_key(), NEXT_EPOCH)
}

/// As chaves de quem forja: de outra `VaultKey`, na época pedida.
fn attacker_keys(epoch: u32) -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([0xAA; 32]), epoch)
}

/// O erro que o dispositivo da época 1 recebe quando o armazenamento mostra a época 2.
fn blocked() -> EngineError {
    EngineError::NeedsNewKey {
        have: 1,
        found: NEXT_EPOCH,
    }
}

struct Lab {
    tmp: Tmp,
    /// O armazenamento compartilhado de `a`, do outro dispositivo e do forjador.
    store: Backend,
    roots: Roots,
    /// O manifest legítimo de `a`, da época 1.
    own: ObjectId,
    with_state: bool,
}

impl Lab {
    /// `a`, época 1, chave velha, com ou sem arquivo de estado.
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

    /// Tudo que o armazenamento guarda, por (tipo, hash), com os bytes.
    async fn stored(&self) -> BTreeMap<(usize, [u8; 32]), Vec<u8>> {
        let mut all = BTreeMap::new();
        for (n, kind) in [ObjectKind::Blob, ObjectKind::Manifest]
            .into_iter()
            .enumerate()
        {
            for id in self.store.list(kind).await.unwrap() {
                let bytes = self.store.get(&id).await.unwrap().unwrap();
                all.insert((n, id.hash), bytes);
            }
        }
        all
    }

    /// O estado de `a` não andou: ainda é o manifest legítimo da época 1.
    fn assert_state_untouched(&self) {
        if self.with_state {
            assert_eq!(
                DeviceState::load(&self.state_path()).unwrap(),
                Some(DeviceState {
                    key_epoch: 1,
                    last_accepted: Hash32(self.own.hash),
                })
            );
        }
    }
}

/// `a` publica o manifest legítimo da época 1 (um arquivo no Vault). Sem estrangeiro nenhum, nada
/// o bloqueia: uma ponta só.
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
    assert_eq!(lab.manifest_count().await, 1);
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    let heads = engine.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, id);
    (lab, engine)
}

/// O atacante grava `count` manifests da `epoch` no armazenamento, cifrados com chave que `a` não
/// tem (corpo distinto: `counter` e nonce), com id distinto.
async fn flood(store: &Backend, count: usize, epoch: u32) -> Vec<ObjectId> {
    let keys = attacker_keys(epoch);
    let mut ids = Vec::with_capacity(count);
    for n in 0..count as u64 {
        let manifest = Manifest {
            format_version: 1,
            key_epoch: epoch,
            base_version: None,
            merged_from: Vec::new(),
            counter: n + 1,
            device: "forjador".to_string(),
            created_at: n,
            files: Vec::new(),
        };
        let sealed = manifest.seal(&keys).unwrap();
        store.put(&sealed.id, &sealed.bytes).await.unwrap();
        ids.push(sealed.id);
    }
    ids
}

/// O outro dispositivo legítimo, `b`, rotaciona com K2' sobre o mesmo armazenamento (1ª tentativa
/// dele, só o Vault, sem estado) e devolve o id do manifest de época 2 que ele publicou.
async fn another_device_rotates(lab: &Lab) -> ObjectId {
    let vault = lab.tmp.0.join("b-vault");
    fs::create_dir_all(&vault).unwrap();
    fs::write(vault.join("b.md"), b"vault de b").unwrap();
    let roots = Roots {
        vault: Some(vault),
        config: None,
        secret: None,
    };
    let other = Engine::new(lab.store.clone(), old_keys(), "b").unwrap();
    let (_, snapshot) = other
        .begin_epoch(&other_vault_key(), &roots, 10)
        .await
        .unwrap();
    let Snapshot::Created { id, manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    assert_eq!(manifest.key_epoch, NEXT_EPOCH);
    assert_eq!(manifest.device, "b");
    assert_eq!(manifest.base_version, None);
    id
}

/// O que um `Engine` com as chaves K2 (as do `next.load()` do `begin_epoch`) enxerga: as pontas
/// que abrem com K2 e os ids ilegíveis. `tips` vazio significa que o `begin_epoch` entra na
/// 1ª tentativa (`own.is_empty()`); `tips` não vazio, na retomada.
async fn what_the_new_keys_see(lab: &Lab) -> (Vec<ObjectId>, HashSet<[u8; 32]>) {
    let probe = Engine::new(lab.store.clone(), k2_keys(), "sonda").unwrap();
    let heads = probe.heads().await.unwrap();
    let tips = heads.tips.iter().map(|t| t.id).collect();
    let unreadable = heads.rejected.iter().map(|r| r.id.hash).collect();
    (tips, unreadable)
}

/// Premissa de (a): `theirs` é um manifest de época 2 no armazenamento que abre com a chave de
/// `author` e **não** abre com K2 nem com as chaves velhas; o `Engine` velho o vê como estrangeiro
/// não reconhecido; e com K2 ele é ilegível e não há ponta, ou seja, o `begin_epoch(K2)` está na
/// 1ª tentativa.
async fn assert_theirs_is_an_epoch_2_object_k2_cannot_open(
    lab: &Lab,
    engine: &Engine<Backend>,
    theirs: &ObjectId,
    author: &EpochKeys,
) {
    let listed = lab.store.list(ObjectKind::Manifest).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&lab.own) && listed.contains(theirs));
    let bytes = lab.store.get(theirs).await.unwrap().unwrap();
    let (header, _) = Header::decode(&bytes).unwrap();
    assert_eq!(header.key_epoch, NEXT_EPOCH);
    assert!(Manifest::open(author, &bytes).is_ok());
    assert!(Manifest::open(&k2_keys(), &bytes).is_err());
    assert!(Manifest::open(&old_keys(), &bytes).is_err());
    assert_eq!(
        engine.foreign_epochs().await.unwrap(),
        vec![ForeignEpoch {
            epoch: NEXT_EPOCH,
            id: *theirs,
        }]
    );
    let (tips, unreadable) = what_the_new_keys_see(lab).await;
    assert!(tips.is_empty(), "K2 já abre um manifest: seria a retomada");
    assert_eq!(unreadable, HashSet::from([theirs.hash]));
}

/// O contrato: `begin_epoch(K2)` na 1ª tentativa recusa, duas vezes seguidas, e não escreve nada
/// (armazenamento byte a byte igual, estado onde estava).
async fn assert_the_first_attempt_is_vetoed_and_writes_nothing(
    lab: &Lab,
    engine: &Engine<Backend>,
) {
    let before = lab.stored().await;
    for round in 0..2 {
        let outcome = engine.begin_epoch(&k2(), &lab.roots, 30).await.map(|_| ());
        assert_eq!(outcome, Err(blocked()), "rodada {round}");
    }
    assert_eq!(lab.stored().await, before);
    lab.assert_state_untouched();
}

/// (a) Outro dispositivo legítimo já rotacionou com K2' (diferente de K2).
async fn a_legitimate_rotation_by_another_device_vetoes_begin_epoch(with_state: bool) {
    let (lab, engine) = setup(with_state).await;
    let theirs = another_device_rotates(&lab).await;
    assert_theirs_is_an_epoch_2_object_k2_cannot_open(&lab, &engine, &theirs, &other_keys()).await;
    assert_the_first_attempt_is_vetoed_and_writes_nothing(&lab, &engine).await;
}

#[tokio::test]
async fn without_state_a_legitimate_rotation_by_another_device_vetoes_begin_epoch() {
    a_legitimate_rotation_by_another_device_vetoes_begin_epoch(false).await;
}

#[tokio::test]
async fn with_state_a_legitimate_rotation_by_another_device_vetoes_begin_epoch() {
    a_legitimate_rotation_by_another_device_vetoes_begin_epoch(true).await;
}

/// (a) O mesmo cenário com um manifest **forjado** de época 2 no lugar: o resultado é idêntico,
/// o que mostra que o veto decide só pelo cabeçalho (e por isso uma correção que ignore época
/// igual à que cria também deixaria de proteger contra a rotação legítima de outro dispositivo).
async fn a_forged_epoch_2_object_gives_the_same_veto(with_state: bool) {
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, 1, NEXT_EPOCH).await;
    assert_theirs_is_an_epoch_2_object_k2_cannot_open(
        &lab,
        &engine,
        &forged[0],
        &attacker_keys(NEXT_EPOCH),
    )
    .await;
    assert_the_first_attempt_is_vetoed_and_writes_nothing(&lab, &engine).await;
}

#[tokio::test]
async fn without_state_a_forged_epoch_2_object_gives_the_same_veto_as_a_legitimate_rotation() {
    a_forged_epoch_2_object_gives_the_same_veto(false).await;
}

#[tokio::test]
async fn with_state_a_forged_epoch_2_object_gives_the_same_veto_as_a_legitimate_rotation() {
    a_forged_epoch_2_object_gives_the_same_veto(true).await;
}

/// Quantos forjados passam do teto de reconhecidos (`MAX_ACKS` = 256, `foreign.rs`, privado ao
/// crate; `foreign_ack_cap.rs` prova a fronteira).
const FORGED: usize = 257;

/// (b) Retomada: uma tentativa anterior de `a` com K2 publicou o manifest de época 2 e caiu antes
/// de gravar o estado (simulada por um `Engine` sem estado, que o `begin_epoch` não grava), e
/// depois 257 forjados de época 2 foram gravados. O `begin_epoch(K2)` toma o caminho da retomada:
/// só o `next.load()` roda, e os 257 não vetam.
async fn resuming_with_257_forged_epoch_2_objects_goes_through(with_state: bool) {
    let (lab, engine) = setup(with_state).await;
    let earlier = Engine::new(lab.store.clone(), old_keys(), "a").unwrap();
    let (_, first) = earlier.begin_epoch(&k2(), &lab.roots, 20).await.unwrap();
    let Snapshot::Created {
        id: published,
        manifest,
        ..
    } = first
    else {
        panic!("esperava Created, veio {first:?}");
    };
    assert_eq!(manifest.key_epoch, NEXT_EPOCH);
    let forged = flood(&lab.store, FORGED, NEXT_EPOCH).await;
    let forged_hashes: HashSet<[u8; 32]> = forged.iter().map(|id| id.hash).collect();
    assert_eq!(forged_hashes.len(), FORGED);
    assert_eq!(lab.manifest_count().await, FORGED + 2);

    // premissa 1: o manifest da tentativa anterior abre com K2 e não com as chaves velhas
    let bytes = lab.store.get(&published).await.unwrap().unwrap();
    assert_eq!(
        Manifest::open(&k2_keys(), &bytes).map(|m| m.device),
        Ok("a".to_string())
    );
    assert!(Manifest::open(&old_keys(), &bytes).is_err());
    // premissa 2: o `Engine` velho está vetado (o `self.load()` do `begin_epoch` daria
    // `NeedsNewKey`): ele vê os 257 forjados e o manifest publicado como época mais nova
    assert_eq!(
        engine.foreign_epochs().await.unwrap().len(),
        FORGED + 1,
        "os forjados e o manifest de K2 são estrangeiros para as chaves velhas"
    );
    assert_eq!(engine.heads().await.map(|_| ()), Err(blocked()));
    // premissa 3: com K2 há uma ponta (o publicado) e 257 ilegíveis: `own` não é vazio, então o
    // caminho é o da retomada
    let (tips, unreadable) = what_the_new_keys_see(&lab).await;
    assert_eq!(tips, vec![published]);
    assert_eq!(unreadable, forged_hashes);

    let before = lab.stored().await;
    let (next, snapshot) = engine.begin_epoch(&k2(), &lab.roots, 30).await.unwrap();
    // a 1ª tentativa publicaria `Created` com `base_version = null`; adotar o que já existe é
    // `Unchanged` com a ponta publicada antes
    match snapshot {
        Snapshot::Unchanged { head, .. } => assert_eq!(head, published),
        other => panic!("esperava Unchanged (retomada), veio {other:?}"),
    }
    assert_eq!(lab.stored().await, before);
    let heads = next.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, published);
    assert_eq!(heads.rejected.len(), FORGED);
    if with_state {
        assert_eq!(
            DeviceState::load(&lab.state_path()).unwrap(),
            Some(DeviceState {
                key_epoch: NEXT_EPOCH,
                last_accepted: Hash32(published.hash),
            })
        );
    }
}

#[tokio::test]
async fn without_state_resuming_with_257_forged_epoch_2_objects_goes_through() {
    resuming_with_257_forged_epoch_2_objects_goes_through(false).await;
}

#[tokio::test]
async fn with_state_resuming_with_257_forged_epoch_2_objects_goes_through() {
    resuming_with_257_forged_epoch_2_objects_goes_through(true).await;
}

/// (b) Controle: os mesmos 257 forjados de época 2 **sem** o manifest da tentativa anterior. A
/// 1ª tentativa roda o `self.load()` das chaves velhas e o veto é permanente (reconhecer tudo
/// deixa 1 sem reconhecer, `foreign_ack_cap.rs`). É a diferença para o teste de cima: o que muda
/// é só haver ou não um manifest que abre com K2.
#[tokio::test]
async fn control_the_same_257_forged_objects_without_the_earlier_manifest_veto_the_first_attempt() {
    let (lab, engine) = setup(false).await;
    let forged = flood(&lab.store, FORGED, NEXT_EPOCH).await;
    let forged_hashes: HashSet<[u8; 32]> = forged.iter().map(|id| id.hash).collect();
    assert_eq!(lab.manifest_count().await, FORGED + 1);
    // premissa: com K2 não há ponta (1ª tentativa) e os 257 são ilegíveis
    let (tips, unreadable) = what_the_new_keys_see(&lab).await;
    assert!(tips.is_empty());
    assert_eq!(unreadable, forged_hashes);

    let before = lab.stored().await;
    for round in 0..2 {
        if round == 1 {
            let shown = engine.foreign_epochs().await.unwrap();
            assert_eq!(shown.len(), FORGED);
            engine.acknowledge_foreign(&shown).await.unwrap();
            assert_eq!(engine.foreign_epochs().await.unwrap().len(), 1);
        }
        let outcome = engine.begin_epoch(&k2(), &lab.roots, 30).await.map(|_| ());
        assert_eq!(outcome, Err(blocked()), "rodada {round}");
    }
    assert_eq!(lab.stored().await, before);
}

/// (d) Um **único** forjado de época 2 no armazenamento. A rotação passa depois de a pessoa
/// reconhecê-lo (sem isso é o veto do item (a)); o `Engine` novo o vê como ilegível da própria
/// época (`rejected`), e o `snapshot` seguinte recusa com `Diverged`. O ADR mediu isto para 256;
/// aqui é para 1.
async fn one_forged_epoch_2_object_makes_the_next_snapshot_diverge(with_state: bool) {
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, 1, NEXT_EPOCH).await;
    assert_theirs_is_an_epoch_2_object_k2_cannot_open(
        &lab,
        &engine,
        &forged[0],
        &attacker_keys(NEXT_EPOCH),
    )
    .await;
    // reconhecido o objeto, a 1ª tentativa publica a época 2
    let shown = engine.foreign_epochs().await.unwrap();
    engine.acknowledge_foreign(&shown).await.unwrap();
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    let (next, snapshot) = engine.begin_epoch(&k2(), &lab.roots, 30).await.unwrap();
    let Snapshot::Created { id, manifest, .. } = snapshot else {
        panic!("esperava Created, veio {snapshot:?}");
    };
    // premissa: foi a 1ª tentativa (sem base) e o armazenamento tem os dois de época 2
    assert_eq!(manifest.key_epoch, NEXT_EPOCH);
    assert_eq!(manifest.base_version, None);
    assert_eq!(manifest.counter, 1);
    assert_eq!(lab.manifest_count().await, 3);

    // o forjado é `rejected` do `Engine` novo (mesma época, não abre), nunca ponta
    let heads = next.heads().await.unwrap();
    assert_eq!(heads.tips.len(), 1);
    assert_eq!(heads.tips[0].id, id);
    assert_eq!(
        heads.rejected.iter().map(|r| r.id).collect::<Vec<_>>(),
        forged
    );
    assert_eq!(
        next.snapshot(&lab.roots, 31).await.map(|_| ()),
        Err(EngineError::Diverged {
            tips: 1,
            rejected: 1
        })
    );
}

#[tokio::test]
async fn without_state_one_forged_epoch_2_object_makes_the_snapshot_after_rotation_diverge() {
    one_forged_epoch_2_object_makes_the_next_snapshot_diverge(false).await;
}

#[tokio::test]
async fn with_state_one_forged_epoch_2_object_makes_the_snapshot_after_rotation_diverge() {
    one_forged_epoch_2_object_makes_the_next_snapshot_diverge(true).await;
}
