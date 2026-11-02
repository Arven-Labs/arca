//! Teste de integração que só usa a API **pública** do `arca-core` (P-32.b).
//!
//! Prova que `Engine::push` e `Engine::pull` bastam para dois dispositivos divergirem, trocarem
//! objetos por um remoto, fundirem e convergirem, sem o `replicate` cru (que não é público).

use arca_core::backend::Backend;
use arca_core::disk::DiskStore;
use arca_core::engine::{Engine, MergeOutcome, Roots, Snapshot};
use arca_core::{EpochKeys, ObjectId, VaultKey};
use std::fs;
use std::path::{Path, PathBuf};

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

struct Device {
    engine: Engine<Backend>,
    roots: Roots,
    vault: PathBuf,
}

fn store(base: &Path, name: &str) -> Backend {
    Backend::Disk(DiskStore::open(base.join(name)).unwrap())
}

fn device(base: &Path, name: &str) -> Device {
    let local = store(base, &format!("{name}-store"));
    let vault = base.join(format!("{name}-vault"));
    fs::create_dir_all(&vault).unwrap();
    let keys = EpochKeys::derive(&VaultKey::from_bytes([3u8; 32]), 1);
    Device {
        engine: Engine::new(local.clone(), keys, name).unwrap(),
        roots: Roots {
            vault: Some(vault.clone()),
            config: None,
            secret: None,
        },
        vault,
    }
}

async fn snapshot(device: &Device, now: u64) -> ObjectId {
    match device.engine.snapshot(&device.roots, now).await.unwrap() {
        Snapshot::Created { id, .. } => id,
        other => panic!("esperava Created, veio {other:?}"),
    }
}

async fn tips(device: &Device) -> Vec<ObjectId> {
    let heads = device.engine.heads().await.unwrap();
    assert!(heads.rejected.is_empty(), "{:?}", heads.rejected);
    heads.tips.iter().map(|tip| tip.id).collect()
}

#[tokio::test]
async fn two_devices_converge_using_only_push_pull_and_merge() {
    let tmp = Tmp::new();
    let remote = store(&tmp.0, "remoto");
    let (a, b) = (device(&tmp.0, "a"), device(&tmp.0, "b"));

    // A publica; B puxa
    fs::write(a.vault.join("comum.md"), b"comum").unwrap();
    let base = snapshot(&a, 1).await;
    a.engine.push(&remote).await.unwrap();
    b.engine.pull(&remote).await.unwrap();
    assert_eq!(tips(&b).await, vec![base]);
    b.engine.apply(None, &base, &b.roots).await.unwrap();

    // divergem offline
    fs::write(a.vault.join("so-a.md"), b"a").unwrap();
    snapshot(&a, 10).await;
    fs::write(b.vault.join("so-b.md"), b"b").unwrap();
    snapshot(&b, 20).await;

    // trocam pelo remoto: cada um fica com duas pontas
    a.engine.push(&remote).await.unwrap();
    b.engine.push(&remote).await.unwrap();
    a.engine.pull(&remote).await.unwrap();
    b.engine.pull(&remote).await.unwrap();
    assert_eq!(tips(&a).await.len(), 2);
    assert_eq!(tips(&b).await.len(), 2);

    // cada um funde sozinho e troca de novo: as fusões gêmeas contam como uma ponta só
    for (device, now) in [(&a, 30), (&b, 31)] {
        match device.engine.merge(now).await.unwrap() {
            MergeOutcome::Merged { tips_after, .. } => assert_eq!(tips_after, 1),
            other => panic!("esperava Merged, veio {other:?}"),
        }
    }
    a.engine.push(&remote).await.unwrap();
    b.engine.push(&remote).await.unwrap();
    a.engine.pull(&remote).await.unwrap();
    b.engine.pull(&remote).await.unwrap();
    let (tips_a, tips_b) = (tips(&a).await, tips(&b).await);
    assert_eq!(tips_a.len(), 1, "{tips_a:?}");
    assert_eq!(tips_a, tips_b);
}
