//! Tipos de objeto e identificador derivado do conteúdo (`SPEC-TECNICA.md` §3).

use crate::error::Error;
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

/// Tipo de objeto guardado num backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ObjectKind {
    /// Um arquivo do Vault, cifrado por inteiro.
    Blob = 1,
    /// O estado do Vault num instante, cifrado.
    Manifest = 2,
}

impl TryFrom<u8> for ObjectKind {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            1 => Ok(ObjectKind::Blob),
            2 => Ok(ObjectKind::Manifest),
            other => Err(Error::UnknownKind(other)),
        }
    }
}

impl ObjectKind {
    /// Nome usado como prefixo da chave do objeto (`blob/…`, `manifest/…`).
    fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Blob => "blob",
            ObjectKind::Manifest => "manifest",
        }
    }
}

/// Identificador de um objeto: tipo + SHA-256 dos **bytes cifrados** (ADR 0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectId {
    pub kind: ObjectKind,
    pub hash: [u8; 32],
}

impl ObjectId {
    /// Calcula o id de um objeto a partir dos seus bytes cifrados.
    pub fn of(kind: ObjectKind, encrypted: &[u8]) -> ObjectId {
        ObjectId {
            kind,
            hash: Sha256::digest(encrypted).into(),
        }
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/", self.kind.as_str())?;
        for byte in self.hash {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ObjectId {
    type Err = Error;

    /// Aceita só `blob/<64 hex minúsculos>` ou `manifest/<64 hex minúsculos>`. O nome vem
    /// de um backend (`list`), então é entrada não confiável: nada de `..`, barras extras
    /// ou maiúsculas.
    fn from_str(s: &str) -> Result<Self, Error> {
        let (kind, hex) = s.split_once('/').ok_or(Error::InvalidObjectId)?;
        let kind = match kind {
            "blob" => ObjectKind::Blob,
            "manifest" => ObjectKind::Manifest,
            _ => return Err(Error::InvalidObjectId),
        };
        let hex = hex.as_bytes();
        if hex.len() != 64 {
            return Err(Error::InvalidObjectId);
        }
        let mut hash = [0u8; 32];
        // `hex.len() == 64`, então os pares cobrem tudo e o resto é vazio.
        let (pairs, _) = hex.as_chunks::<2>();
        for (out, [hi, lo]) in hash.iter_mut().zip(pairs) {
            *out = (hex_digit(*hi)? << 4) | hex_digit(*lo)?;
        }
        Ok(ObjectId { kind, hash })
    }
}

/// Um dígito hexadecimal **minúsculo**.
fn hex_digit(c: u8) -> Result<u8, Error> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(Error::InvalidObjectId),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn object_kind_from_u8_accepts_only_1_and_2() {
        assert_eq!(ObjectKind::try_from(1), Ok(ObjectKind::Blob));
        assert_eq!(ObjectKind::try_from(2), Ok(ObjectKind::Manifest));
        for v in [0u8, 3, 4, 127, 255] {
            assert_eq!(ObjectKind::try_from(v), Err(Error::UnknownKind(v)));
        }
    }

    #[test]
    fn id_is_sha256_of_the_encrypted_bytes() {
        let id = ObjectId::of(ObjectKind::Blob, b"abc");
        assert_eq!(id.to_string(), format!("blob/{ABC_SHA256}"));
    }

    #[test]
    fn same_bytes_same_id_different_bytes_different_id() {
        let a = ObjectId::of(ObjectKind::Blob, b"hello");
        assert_eq!(a, ObjectId::of(ObjectKind::Blob, b"hello"));
        assert_ne!(a, ObjectId::of(ObjectKind::Blob, b"hellp"));
        assert_ne!(a.hash, ObjectId::of(ObjectKind::Blob, b"hello!").hash);
    }

    #[test]
    fn kind_is_part_of_the_id() {
        let blob = ObjectId::of(ObjectKind::Blob, b"x");
        let manifest = ObjectId::of(ObjectKind::Manifest, b"x");
        assert_eq!(blob.hash, manifest.hash);
        assert_ne!(blob, manifest);
        assert!(manifest.to_string().starts_with("manifest/"));
    }

    #[test]
    fn display_then_parse_round_trips() {
        for kind in [ObjectKind::Blob, ObjectKind::Manifest] {
            for data in [&b""[..], b"a", b"some longer payload"] {
                let id = ObjectId::of(kind, data);
                assert_eq!(id.to_string().parse::<ObjectId>(), Ok(id));
            }
        }
    }

    #[test]
    fn parse_rejects_malformed_names() {
        let ok = format!("blob/{ABC_SHA256}");
        assert!(ok.parse::<ObjectId>().is_ok());

        let upper = format!("blob/{}", ABC_SHA256.to_uppercase());
        let short = format!("blob/{}", &ABC_SHA256[..63]);
        let long = format!("blob/{ABC_SHA256}0");
        let non_hex = format!("blob/{}", ABC_SHA256.replacen('b', "g", 1));
        let bad = [
            String::new(),
            "blob".to_string(),
            "blob/".to_string(),
            format!("/{ABC_SHA256}"),
            format!("blob//{ABC_SHA256}"),
            format!("blob/{ABC_SHA256}/"),
            format!("blob/{ABC_SHA256}/extra"),
            format!("blob/../{ABC_SHA256}"),
            format!("../blob/{ABC_SHA256}"),
            format!("Blob/{ABC_SHA256}"),
            format!("chunk/{ABC_SHA256}"),
            format!("blob/ {ABC_SHA256}"),
            format!("blob/{ABC_SHA256} "),
            format!("blob/{ABC_SHA256}\n"),
            format!("blob/{}\0", &ABC_SHA256[..63]),
            format!("blob\\{ABC_SHA256}"),
            upper,
            short,
            long,
            non_hex,
        ];
        for name in bad {
            assert_eq!(
                name.parse::<ObjectId>(),
                Err(Error::InvalidObjectId),
                "should reject {name:?}"
            );
        }
    }
}
