//! O teto de reconhecidos (`MAX_ACKS` = 256, `foreign.rs`) nos chamadores de `load()` que
//! `foreign_ack_cap.rs` só dava "por leitura" (ADR 0018, "Ainda não corrigido, por escolha",
//! fato 2): `verify`, `conflicts_between`, `merge_with_losers` e `snapshot_allowing_empty`. Este
//! arquivo torna isso fato: um teste por operação.
//!
//! Cenário: o dispositivo `a` (época 1, sem estado) publicou um manifest legítimo; o armazenamento
//! recebe N manifests de época 2 cifrados com chave que `a` não tem. Para cada operação:
//! - com **257** forjados, ela dá `NeedsNewKey { have: 1, found: 2 }`, e reconhecer tudo (duas
//!   vezes) deixa exatamente 1 sem reconhecer, então continua dando o mesmo erro;
//! - com **256** (cenário novo, o mesmo código), reconhecer tudo destrava: a operação roda e dá o
//!   resultado normal de um armazenamento com uma ponta só (afirmado em `call`).
//!
//! Cada cenário afirma antes a **premissa**: o backend lista 1 + N manifests, os N têm id
//! distinto, cabeçalho de época 2 e não abrem nem com as chaves de `a` nem com as da época 2; e
//! `a` os vê todos como estrangeiros não reconhecidos (o que prova que a operação chegou ao caminho
//! do teto, e não a outro erro). A operação ser vetada por `load()` (e não por outra checagem) é o
//! que o 257 vs. 256 prova: só o número de forjados muda.
//!
//! Resposta, confirmada no CI (run 46083349358): os quatro testes passam. São
//! caracterização do comportamento atual: ficam vermelhos no dia em que a operação deixar de
//! chamar `load()` (o 257 deixa de vetar) ou em que o teto deixar de travar; é esse o sinal para
//! atualizar o ADR 0018.

mod common;

use arca_core::backend::Backend;
use arca_core::disk::DiskStore;
use arca_core::engine::{
    Engine, EngineError, ForeignEpoch, MergeOutcome, Roots, SavedLosers, Snapshot, VerifyMode,
};
use arca_core::manifest::Manifest;
use arca_core::store::BlobStore;
use arca_core::{EpochKeys, Header, ObjectId, ObjectKind, VaultKey};
use common::{flood, old_keys, Tmp};
use std::collections::HashSet;
use std::fs;

/// A época que o atacante grava: maior que a do dispositivo (1).
const FOREIGN_EPOCH: u32 = 2;

/// As chaves que uma rotação de `a` usaria (época 2): os forjados também não abrem com elas.
fn next_keys() -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([4u8; 32]), 2)
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
}

impl Lab {
    async fn manifest_count(&self) -> usize {
        self.store.list(ObjectKind::Manifest).await.unwrap().len()
    }
}

/// `a` publica o manifest legítimo da época 1 (um arquivo no Vault), o engine sem estado.
async fn setup() -> (Lab, Engine<Backend>) {
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
    let engine = Engine::new(store.clone(), old_keys(), "a").unwrap();
    let Snapshot::Created { id, .. } = engine.snapshot(&roots, 1).await.unwrap() else {
        panic!("esperava Created");
    };
    // controle: sem inundação, nada bloqueia
    assert_eq!(store.list(ObjectKind::Manifest).await.unwrap(), vec![id]);
    assert!(engine.foreign_epochs().await.unwrap().is_empty());
    let lab = Lab {
        tmp,
        store,
        roots,
        own: id,
    };
    (lab, engine)
}

/// O cenário: `a` + `count` forjados, com a premissa afirmada (ver o cabeçalho do arquivo).
async fn scene(count: usize) -> (Lab, Engine<Backend>) {
    let (lab, engine) = setup().await;
    let forged = flood(&lab.store, count, FOREIGN_EPOCH).await;
    let listed = lab.store.list(ObjectKind::Manifest).await.unwrap();
    assert_eq!(listed.len(), count + 1);
    assert!(listed.contains(&lab.own));
    let distinct: HashSet<[u8; 32]> = forged.iter().map(|id| id.hash).collect();
    assert_eq!(distinct.len(), count);
    for id in &forged {
        assert!(listed.contains(id));
        let bytes = lab.store.get(id).await.unwrap().unwrap();
        let (header, _) = Header::decode(&bytes).unwrap();
        assert_eq!(header.key_epoch, FOREIGN_EPOCH);
        assert!(Manifest::open(&old_keys(), &bytes).is_err());
        assert!(Manifest::open(&next_keys(), &bytes).is_err());
    }
    let seen = engine.foreign_epochs().await.unwrap();
    assert_eq!(seen.len(), count);
    assert!(seen.iter().all(|f| distinct.contains(&f.id.hash)));
    assert!(seen.iter().all(|f| f.epoch == FOREIGN_EPOCH));
    (lab, engine)
}

/// O que sobra depois de `acknowledge_foreign` de tudo que `foreign_epochs` mostrava.
async fn acknowledge_everything(engine: &Engine<Backend>) -> Vec<ForeignEpoch> {
    let shown = engine.foreign_epochs().await.unwrap();
    engine.acknowledge_foreign(&shown).await.unwrap();
    engine.foreign_epochs().await.unwrap()
}

/// As quatro operações que `foreign_ack_cap.rs` só dava por leitura.
#[derive(Clone, Copy, Debug)]
enum Op {
    Verify,
    ConflictsBetween,
    MergeWithLosers,
    SnapshotAllowingEmpty,
}

/// Chama a operação. `Err` é o resultado de `load()`; `Ok(())` só sai depois de afirmar o
/// resultado normal de um armazenamento com uma ponta só (a de `a`), sem nada escrito.
async fn call(op: Op, lab: &Lab, engine: &Engine<Backend>) -> Result<(), EngineError> {
    match op {
        Op::Verify => {
            let report = engine.verify(VerifyMode::Presence).await?;
            assert_eq!(report.manifests_ok, 1);
            assert!(report.rejected.is_empty() && report.missing.is_empty());
            assert!(report.clean);
        }
        Op::ConflictsBetween => {
            let found = engine.conflicts_between(None, &lab.own).await?;
            assert!(found.is_empty());
        }
        Op::MergeWithLosers => {
            let dir = lab.tmp.0.join("losers");
            fs::create_dir_all(&dir).unwrap();
            let (outcome, saved) = engine.merge_with_losers(20, &dir, &lab.roots).await?;
            assert_eq!(
                outcome,
                MergeOutcome::Nothing {
                    head: Some(lab.own)
                }
            );
            assert_eq!(
                saved,
                SavedLosers {
                    saved: 0,
                    already: 0
                }
            );
        }
        Op::SnapshotAllowingEmpty => {
            match engine.snapshot_allowing_empty(&lab.roots, 21, &[]).await? {
                Snapshot::Unchanged { head, .. } => assert_eq!(head, lab.own),
                other => panic!("esperava Unchanged, veio {other:?}"),
            }
        }
    }
    Ok(())
}

/// 257 vetam, mesmo reconhecendo tudo (duas rodadas, sobra sempre 1); 256 destravam com um
/// reconhecimento. Em nenhum caso a operação escreve no armazenamento.
async fn the_cap_decides_whether_the_operation_runs(op: Op) {
    const OVER: usize = 257;
    const AT: usize = 256;

    let (lab, engine) = scene(OVER).await;
    assert_eq!(call(op, &lab, &engine).await, Err(blocked()), "{op:?}, 257");
    for round in 0..2 {
        let left = acknowledge_everything(&engine).await;
        assert_eq!(left.len(), 1, "{op:?}, rodada {round}: {left:?}");
        assert_eq!(call(op, &lab, &engine).await, Err(blocked()), "{op:?}, 257");
    }
    assert_eq!(lab.manifest_count().await, OVER + 1);

    let (lab, engine) = scene(AT).await;
    assert_eq!(call(op, &lab, &engine).await, Err(blocked()), "{op:?}, 256");
    assert!(acknowledge_everything(&engine).await.is_empty());
    assert_eq!(call(op, &lab, &engine).await, Ok(()), "{op:?}, 256");
    assert_eq!(lab.manifest_count().await, AT + 1);
}

#[tokio::test]
async fn verify_is_vetoed_by_257_foreign_manifests_and_runs_with_256_acknowledged() {
    the_cap_decides_whether_the_operation_runs(Op::Verify).await;
}

#[tokio::test]
async fn conflicts_between_is_vetoed_by_257_foreign_manifests_and_runs_with_256_acknowledged() {
    the_cap_decides_whether_the_operation_runs(Op::ConflictsBetween).await;
}

#[tokio::test]
async fn merge_with_losers_is_vetoed_by_257_foreign_manifests_and_runs_with_256_acknowledged() {
    the_cap_decides_whether_the_operation_runs(Op::MergeWithLosers).await;
}

#[tokio::test]
async fn snapshot_allowing_empty_is_vetoed_by_257_foreign_manifests_and_runs_with_256_acknowledged()
{
    the_cap_decides_whether_the_operation_runs(Op::SnapshotAllowingEmpty).await;
}
