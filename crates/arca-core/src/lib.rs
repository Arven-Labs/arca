//! Núcleo da arca: camada local-first de armazenamento e sincronização com
//! criptografia no cliente. Esqueleto da Fase 1; a lógica entra nos PRs seguintes.
#![forbid(unsafe_code)]

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
