//! O teto de reconhecimentos (`MAX_ACKS` = 256, `foreign.rs`) contra um armazenamento hostil
//! (P-49, Sessão 131, ADR 0018 "Ainda não corrigido, por escolha"). Pergunta: um `Engine` ligado
//! **direto** a um backend que lista mais de 256 manifests de época mais nova, que ele não abre,
//! fica travado para sempre? O `apply` agora chama `load()` sempre (arca#211), então ele entra na
//! pergunta junto com `heads`, `merge` e `snapshot`.
//!
//! Por que o teto é alcançável: `load()` lê **todos** os manifests do armazenamento do `Engine`
//! (`scan_manifests`) e, para cada um de época maior que a das chaves, `unacknowledged` só o
//! descarta se o par `(época, id)` está nos reconhecidos. `ForeignAcks::add` guarda no máximo 256
//! pares e, ao passar disso, tira o mais antigo. Com 300 manifests estrangeiros, reconhecer os 300
//! deixa 256 e **desreconhece 44**, que bloqueiam de novo; reconhecer esses 44 tira outros 44.
//! Nada no `Engine` apaga objeto do armazenamento, então a saída não existe pela API do núcleo.
//! (O `pull` não grava manifest estrangeiro na réplica; este teste é o caso do `Engine` **sem**
//! réplica local, o limite que o ADR 0018 já registra em "Não validado".)
//!
//! Cenário: o dispositivo `a` (época 1, chave velha) publica um manifest legítimo com um arquivo
//! de Vault. Um atacante grava no **mesmo** armazenamento N manifests de época 2, cifrados com uma
//! chave que `a` não tem (ele também não precisa da chave certa: o cabeçalho em claro basta, ADR
//! 0018). Cada teste afirma antes a **premissa** (o backend lista 1 + N manifests; os N têm id
//! distinto, cabeçalho de época 2 e não abrem com as chaves de `a`; `a` os vê todos como
//! estrangeiros não reconhecidos, o que prova que o `Engine` chegou ao caminho do teto) e só
//! depois o **resultado**.
//!
//! Resposta esperada, que o CI confirma: com mais de 256, `heads`, `merge`, `snapshot` e `apply`
//! dão `NeedsNewKey { have: 1, found: 2 }` depois de qualquer número de reconhecimentos, com e sem
//! `with_state` (e depois de reiniciar o `Engine`); com exatamente 256, um reconhecimento basta; com
//! 257, sobra exatamente um não reconhecido, até alguém **apagar do backend** (fora da API do
//! `Engine`) um dos objetos. Isto é caracterização do comportamento atual, não uma correção: os
//! testes `..._stay_blocked_...` ficam vermelhos no dia em que o teto deixar de travar (por exemplo,
//! `unacknowledged` passar a contar só o que cabe, ou o reconhecimento ganhar outra forma), e é
//! esse o sinal para atualizar o ADR 0018.

use arca_core::backend::Backend;
use arca_core::disk::DiskStore;
use arca_core::engine::{Applied, Engine, EngineError, ForeignEpoch, Roots, Snapshot};
use arca_core::manifest::{Hash32, Manifest};
use arca_core::state::DeviceState;
use arca_core::store::BlobStore;
use arca_core::{EpochKeys, Header, ObjectId, ObjectKind, VaultKey};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// O valor de `MAX_ACKS` (`foreign.rs`, privado ao crate). Os testes de fronteira (256 destrava,
/// 257 trava) são o que prova que é este o valor: se mudar, eles falham.
const ACK_CAP: usize = 256;

/// A época que o atacante grava: maior que a do dispositivo (1).
const FOREIGN_EPOCH: u32 = 2;

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

/// As chaves de quem forja: de outra `VaultKey`, na época 2. `a` nunca as tem.
fn attacker_keys() -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([0xAA; 32]), FOREIGN_EPOCH)
}

/// O erro que bloqueia: o dispositivo está na época 1 e o armazenamento mostra a 2.
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

/// O atacante grava `count` manifests da época 2 no armazenamento. Cada um é um manifest válido
/// para quem tem a chave dele (corpo distinto: `counter` e nonce), com id distinto.
async fn flood(store: &Backend, count: usize) -> Vec<ObjectId> {
    let keys = attacker_keys();
    let mut ids = Vec::with_capacity(count);
    for n in 0..count as u64 {
        let manifest = Manifest {
            format_version: 1,
            key_epoch: FOREIGN_EPOCH,
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

/// A premissa: o backend lista o manifest de `a` e os `flood.len()` forjados; os forjados têm id
/// distinto, cabeçalho de época 2 e **não abrem** com as chaves de `a`; e `a` os enxerga todos
/// como estrangeiros não reconhecidos (é o que leva `load()` até o teto).
async fn assert_the_engine_reaches_the_cap_path(
    lab: &Lab,
    engine: &Engine<Backend>,
    flood: &[ObjectId],
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
        assert_eq!(header.key_epoch, FOREIGN_EPOCH);
        assert!(Manifest::open(&old_keys(), &bytes).is_err());
    }
    let seen = engine.foreign_epochs().await.unwrap();
    assert_eq!(seen.len(), flood.len());
    assert!(seen.iter().all(|f| distinct.contains(&f.id.hash)));
    assert!(seen.iter().all(|f| f.epoch == FOREIGN_EPOCH));
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
/// depois de reiniciar o `Engine`.
async fn above_the_cap_the_engine_stays_blocked(with_state: bool) {
    const FLOOD: usize = 300;
    const _: () = assert!(FLOOD > ACK_CAP);
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, FLOOD).await;
    let forged_hashes: HashSet<[u8; 32]> = forged.iter().map(|id| id.hash).collect();
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged).await;
    assert_blocked(&lab, &engine).await;

    // rodada 1: reconhecer tudo, em lotes de 50 (o lote não muda o teto)
    for chunk in engine.foreign_epochs().await.unwrap().chunks(50) {
        engine.acknowledge_foreign(chunk).await.unwrap();
    }
    let left = engine.foreign_epochs().await.unwrap();
    assert_eq!(left.len(), FLOOD - ACK_CAP);
    assert!(left.iter().all(|f| forged_hashes.contains(&f.id.hash)));
    assert_blocked(&lab, &engine).await;

    // rodadas 2 e 3: reconhecer o que sobrou desreconhece outros tantos
    for _ in 0..2 {
        let left = acknowledge_everything(&engine).await;
        assert_eq!(left.len(), FLOOD - ACK_CAP);
        assert!(left.iter().all(|f| forged_hashes.contains(&f.id.hash)));
        assert_blocked(&lab, &engine).await;
    }

    if with_state {
        // reiniciar o app: engine novo, mesmo armazenamento e mesmo arquivo de estado. Os 256
        // reconhecidos estão em disco (`<estado>.foreign`) e os outros 44 continuam bloqueando
        let restarted = lab.engine();
        assert_eq!(
            restarted.foreign_epochs().await.unwrap().len(),
            FLOOD - ACK_CAP
        );
        assert_blocked(&lab, &restarted).await;
        let left = acknowledge_everything(&restarted).await;
        assert_eq!(left.len(), FLOOD - ACK_CAP);
        assert_blocked(&lab, &restarted).await;
    }
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

/// Controle da fronteira: com exatamente 256, um reconhecimento basta e tudo volta a funcionar.
/// Sem isto, o teste de cima poderia estar vermelho por outro motivo que não o teto.
async fn at_the_cap_one_acknowledgement_unblocks(with_state: bool) {
    let (lab, engine) = setup(with_state).await;
    let forged = flood(&lab.store, ACK_CAP).await;
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged).await;
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
    let forged = flood(&lab.store, ACK_CAP + 1).await;
    assert_the_engine_reaches_the_cap_path(&lab, &engine, &forged).await;
    assert_blocked(&lab, &engine).await;

    let mut left = Vec::new();
    for _ in 0..3 {
        left = acknowledge_everything(&engine).await;
        assert_eq!(left.len(), 1, "{left:?}");
        assert_blocked(&lab, &engine).await;
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
