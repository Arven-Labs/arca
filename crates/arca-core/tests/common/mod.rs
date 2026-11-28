//! Helpers que os testes de integração do teto de reconhecidos compartilham
//! (`foreign_ack_cap.rs`, `foreign_ack_cap_callers.rs`, `begin_epoch_foreign_contract.rs`).
//! `tests/common/mod.rs` (e não `tests/common.rs`) não vira binário de teste próprio; cada arquivo
//! de `tests/` o inclui com `mod common;`. Tudo aqui é usado pelos três binários (senão o
//! `dead_code` derruba o clippy com `-D warnings`): ao acrescentar um helper, ele precisa ser
//! usado por todos eles.

use arca_core::backend::Backend;
use arca_core::manifest::Manifest;
use arca_core::store::BlobStore;
use arca_core::{EpochKeys, ObjectId, VaultKey};
use std::fs;
use std::path::PathBuf;

/// Pasta única em `temp_dir()`, removida no `Drop` (o `testutil` do crate não é visível aqui).
pub struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new() -> Tmp {
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

pub fn old_keys() -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([3u8; 32]), 1)
}

/// As chaves de quem forja: de outra `VaultKey`, na época pedida. `a` nunca as tem.
pub fn attacker_keys(epoch: u32) -> EpochKeys {
    EpochKeys::derive(&VaultKey::from_bytes([0xAA; 32]), epoch)
}

/// O atacante grava `count` manifests da `epoch` no armazenamento, cifrados com chave que `a` não
/// tem. Cada um é um manifest válido para quem tem a chave dele (corpo distinto: `counter` e
/// nonce), com id distinto.
pub async fn flood(store: &Backend, count: usize, epoch: u32) -> Vec<ObjectId> {
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
