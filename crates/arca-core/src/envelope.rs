//! Cabeçalho do envelope: em claro, mas autenticado como AAD da cifra (ADR 0009).
//!
//! Layout binário fixo, 36 bytes, inteiros big-endian (`SPEC-TECNICA.md` §5.1):
//!
//! ```text
//! magic "ARCA" (4) | format_version u16 | alg u8 | kind u8 | key_epoch u32 | nonce (24)
//! ```

use crate::error::Error;
use crate::object::ObjectKind;
use crate::FORMAT_VERSION;

/// Tamanho do cabeçalho codificado, em bytes.
pub const HEADER_LEN: usize = 36;
/// Tamanho do nonce do XChaCha20-Poly1305, em bytes.
pub const NONCE_LEN: usize = 24;
/// Bytes que abrem todo objeto da arca.
pub const MAGIC: [u8; 4] = *b"ARCA";

/// Algoritmo do corpo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Alg {
    XChaCha20Poly1305 = 1,
}

impl TryFrom<u8> for Alg {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            1 => Ok(Alg::XChaCha20Poly1305),
            other => Err(Error::UnknownAlg(other)),
        }
    }
}

/// Cabeçalho de um objeto cifrado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub format_version: u16,
    pub alg: Alg,
    pub kind: ObjectKind,
    pub key_epoch: u32,
    pub nonce: [u8; NONCE_LEN],
}

impl Header {
    /// Codifica no layout fixo de 36 bytes.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&self.format_version.to_be_bytes());
        out[6] = self.alg as u8;
        out[7] = self.kind as u8;
        out[8..12].copy_from_slice(&self.key_epoch.to_be_bytes());
        out[12..HEADER_LEN].copy_from_slice(&self.nonce);
        out
    }

    /// Lê o cabeçalho do início de `bytes` e devolve também o resto (o corpo).
    /// Recusa magic errado, entrada curta e valores desconhecidos; nunca interpreta parcialmente.
    pub fn decode(bytes: &[u8]) -> Result<(Header, &[u8]), Error> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Truncated {
                needed: HEADER_LEN,
                got: bytes.len(),
            });
        }
        let (head, body) = bytes.split_at(HEADER_LEN);
        if head[0..4] != MAGIC {
            return Err(Error::BadMagic);
        }
        let format_version = u16::from_be_bytes([head[4], head[5]]);
        if u32::from(format_version) != FORMAT_VERSION {
            return Err(Error::UnknownFormatVersion(format_version));
        }
        let alg = Alg::try_from(head[6])?;
        let kind = ObjectKind::try_from(head[7])?;
        let key_epoch = u32::from_be_bytes([head[8], head[9], head[10], head[11]]);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&head[12..HEADER_LEN]);
        Ok((
            Header {
                format_version,
                alg,
                kind,
                key_epoch,
                nonce,
            },
            body,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FORMAT_VERSION;

    fn sample(kind: ObjectKind, key_epoch: u32, nonce_byte: u8) -> Header {
        Header {
            format_version: FORMAT_VERSION as u16,
            alg: Alg::XChaCha20Poly1305,
            kind,
            key_epoch,
            nonce: [nonce_byte; NONCE_LEN],
        }
    }

    /// Vetor fixo escrito à mão. Se o layout mudar, este teste quebra: o cabeçalho
    /// inteiro é AAD da cifra, então o layout é contrato de formato.
    #[test]
    fn golden_vector() {
        let mut nonce = [0u8; NONCE_LEN];
        for (i, b) in nonce.iter_mut().enumerate() {
            *b = 0xA0 + i as u8;
        }
        let header = Header {
            format_version: 1,
            alg: Alg::XChaCha20Poly1305,
            kind: ObjectKind::Manifest,
            key_epoch: 0x0102_0304,
            nonce,
        };
        #[rustfmt::skip]
        let expected: [u8; 36] = [
            0x41, 0x52, 0x43, 0x41,             // "ARCA"
            0x00, 0x01,                         // format_version = 1 (big-endian)
            0x01,                               // alg = xchacha20poly1305
            0x02,                               // kind = manifest
            0x01, 0x02, 0x03, 0x04,             // key_epoch (big-endian)
            0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
            0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF,
            0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7,
        ];
        assert_eq!(header.encode(), expected);
        assert_eq!(HEADER_LEN, expected.len());
    }

    #[test]
    fn round_trip() {
        for kind in [ObjectKind::Blob, ObjectKind::Manifest] {
            for epoch in [0, 1, 255, 256, u32::MAX] {
                for nonce_byte in [0u8, 1, 0xFF] {
                    let h = sample(kind, epoch, nonce_byte);
                    let bytes = h.encode();
                    let (back, rest) = Header::decode(&bytes).unwrap();
                    assert_eq!(back, h);
                    assert!(rest.is_empty());
                }
            }
        }
    }

    #[test]
    fn decode_returns_the_body_after_the_header() {
        let h = sample(ObjectKind::Blob, 3, 9);
        let mut bytes = h.encode().to_vec();
        bytes.extend_from_slice(b"ciphertext-and-tag");
        let (back, body) = Header::decode(&bytes).unwrap();
        assert_eq!(back, h);
        assert_eq!(body, b"ciphertext-and-tag");
    }

    #[test]
    fn truncated_at_every_length_below_36() {
        let full = sample(ObjectKind::Blob, 1, 1).encode();
        for len in 0..HEADER_LEN {
            assert_eq!(
                Header::decode(&full[..len]),
                Err(Error::Truncated {
                    needed: HEADER_LEN,
                    got: len
                }),
                "length {len}"
            );
        }
    }

    #[test]
    fn bad_magic_is_rejected() {
        let good = sample(ObjectKind::Blob, 1, 1).encode();

        let mut flipped = good;
        flipped[0] ^= 0x01;
        assert_eq!(Header::decode(&flipped), Err(Error::BadMagic));

        let mut other = good;
        other[..4].copy_from_slice(b"WRDN");
        assert_eq!(Header::decode(&other), Err(Error::BadMagic));

        // um bundle JSON do warden-sync (ou qualquer texto) tem de falhar por magic, não por tag
        let json = br#"{"version":1,"files":[],"padding":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
        assert!(json.len() >= HEADER_LEN);
        assert_eq!(Header::decode(json), Err(Error::BadMagic));
    }

    #[test]
    fn unknown_format_version_is_rejected() {
        for v in [0u16, 2, 3, 256, 65535] {
            let mut b = sample(ObjectKind::Blob, 1, 1).encode();
            b[4..6].copy_from_slice(&v.to_be_bytes());
            assert_eq!(Header::decode(&b), Err(Error::UnknownFormatVersion(v)));
        }
    }

    #[test]
    fn unknown_alg_is_rejected() {
        for v in [0u8, 2, 3, 255] {
            let mut b = sample(ObjectKind::Blob, 1, 1).encode();
            b[6] = v;
            assert_eq!(Header::decode(&b), Err(Error::UnknownAlg(v)));
        }
    }

    #[test]
    fn unknown_kind_is_rejected() {
        for v in [0u8, 3, 4, 255] {
            let mut b = sample(ObjectKind::Blob, 1, 1).encode();
            b[7] = v;
            assert_eq!(Header::decode(&b), Err(Error::UnknownKind(v)));
        }
    }

    #[test]
    fn alg_from_u8_accepts_only_1() {
        assert_eq!(Alg::try_from(1), Ok(Alg::XChaCha20Poly1305));
        for v in [0u8, 2, 255] {
            assert_eq!(Alg::try_from(v), Err(Error::UnknownAlg(v)));
        }
    }
}
