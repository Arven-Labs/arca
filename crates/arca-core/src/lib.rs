//! Núcleo da arca: camada local-first de armazenamento e sincronização com
//! criptografia no cliente. A lógica entra aos poucos, um PR por intenção (Fase 1).
#![forbid(unsafe_code)]

pub mod backend;
pub mod crypto;
pub mod disk;
pub mod engine;
pub mod envelope;
pub mod error;
pub mod heads;
pub mod manifest;
pub mod merge;
pub mod object;
pub mod recovery;
pub mod scan;
pub mod store;

#[cfg(test)]
mod testutil;

pub use crypto::{open, seal, EpochKeys, Sealed, VaultKey};
pub use envelope::{Alg, Header, HEADER_LEN, MAGIC, NONCE_LEN};
pub use error::Error;
pub use object::{ObjectId, ObjectKind};

/// Versão do formato da arca gravada nos objetos (ver `project/SPEC-TECNICA.md` §5.1).
pub const FORMAT_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    /// Teste de fumaça: prova que o CI compila e roda testes. Não testa comportamento.
    #[test]
    fn format_version_starts_at_one() {
        assert_eq!(FORMAT_VERSION, 1);
    }
}
