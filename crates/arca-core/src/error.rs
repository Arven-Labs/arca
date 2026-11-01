//! Erros do núcleo. Nenhuma variante carrega segredo nem o conteúdo de um objeto.

/// Erros ao interpretar entrada externa (bytes de objeto, nomes vindos de um backend).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// O objeto não começa com o magic `ARCA`: não é um objeto da arca.
    #[error("not an arca object (bad magic)")]
    BadMagic,
    /// Menos bytes do que o cabeçalho exige.
    #[error("object truncated: header needs {needed} bytes, got {got}")]
    Truncated { needed: usize, got: usize },
    /// `format_version` que esta versão não conhece: recusa, nunca interpreta parcialmente.
    #[error("unknown format_version {0}")]
    UnknownFormatVersion(u16),
    /// `alg` desconhecido.
    #[error("unknown alg {0}")]
    UnknownAlg(u8),
    /// `kind` desconhecido.
    #[error("unknown object kind {0}")]
    UnknownKind(u8),
    /// Falha de autenticação ao decifrar: chave errada, objeto adulterado ou corpo curto demais.
    /// Sem detalhe de propósito, para não servir de oráculo.
    #[error("decryption failed")]
    Decrypt,
    /// A cifra recusou a entrada (grande demais para o algoritmo).
    #[error("encryption failed")]
    Encrypt,
    /// O objeto não é do tipo que o chamador esperava.
    #[error("object kind does not match the expected kind")]
    KindMismatch,
    /// O objeto foi cifrado numa época diferente da das chaves usadas.
    #[error("key epoch {found} does not match expected epoch {expected}")]
    WrongKeyEpoch { found: u32, expected: u32 },
    /// O sistema operacional não forneceu bytes aleatórios; nunca se usa um nonce fraco.
    #[error("operating system random number generator failed")]
    Random,
    /// Nome de objeto fora do formato `<kind>/<64 hex minúsculos>`.
    #[error("invalid object id")]
    InvalidObjectId,
}
