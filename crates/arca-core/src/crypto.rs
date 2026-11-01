//! Derivação de chaves e cifra dos objetos (ADR 0008 e 0009, `SPEC-TECNICA.md` §5).
//!
//! - Chave-raiz: a `vault_key` de 32 bytes.
//! - HKDF-SHA256 com `salt = "arca"` e `info = "arca/v1/<tipo>/<época>"` dá duas subchaves
//!   independentes por época: uma para blobs e outra para manifests.
//! - XChaCha20-Poly1305 com nonce aleatório de 24 bytes por objeto. O cabeçalho inteiro de
//!   36 bytes (incluindo o nonce) é o AAD: alterá-lo invalida a tag.
//!
//! Limite conhecido: `zeroize` apaga o que está nos tipos daqui (`Zeroizing`); cópias
//! internas do crate de cifra (registradores, pilha) não têm essa garantia.

use crate::envelope::{Alg, Header, HEADER_LEN, NONCE_LEN};
use crate::error::Error;
use crate::object::{ObjectId, ObjectKind};
use crate::FORMAT_VERSION;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use std::fmt;
use zeroize::Zeroizing;

/// Salt do HKDF da arca. Os do `warden-sync`, do pareamento, do `warden-core` e do TruthID
/// são outros (ver os testes de separação de domínio).
const HKDF_SALT: &[u8] = b"arca";
/// Tamanho da tag Poly1305, em bytes.
const TAG_LEN: usize = 16;

/// A `vault_key`: segredo-raiz de 32 bytes. Nunca aparece em `Debug`.
pub struct VaultKey(Zeroizing<[u8; 32]>);

impl VaultKey {
    /// Constrói a partir de 32 bytes (o chamador é dono de apagar a cópia que passou).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        VaultKey(Zeroizing::new(bytes))
    }

    /// Chave nova: 32 bytes aleatórios do sistema operacional (o mesmo gerador dos nonces).
    /// Se o SO não fornecer, `Error::Random`: nunca uma chave fraca.
    pub fn generate() -> Result<VaultKey, Error> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut bytes[..]).map_err(|_| Error::Random)?;
        Ok(VaultKey::from_bytes(*bytes))
    }

    /// Os bytes da chave, para a codificação da frase de recuperação.
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VaultKey(<redacted>)")
    }
}

/// As duas subchaves de uma época.
pub struct EpochKeys {
    epoch: u32,
    blob: Zeroizing<[u8; 32]>,
    manifest: Zeroizing<[u8; 32]>,
}

impl EpochKeys {
    /// Deriva as subchaves de `blob` e `manifest` da época `key_epoch`.
    pub fn derive(vault_key: &VaultKey, key_epoch: u32) -> Self {
        EpochKeys {
            epoch: key_epoch,
            blob: derive_key(&vault_key.0[..], ObjectKind::Blob, key_epoch),
            manifest: derive_key(&vault_key.0[..], ObjectKind::Manifest, key_epoch),
        }
    }

    /// A época a que estas chaves pertencem.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    fn key_for(&self, kind: ObjectKind) -> &[u8; 32] {
        match kind {
            ObjectKind::Blob => &self.blob,
            ObjectKind::Manifest => &self.manifest,
        }
    }
}

/// HKDF-SHA256 com `salt = "arca"` e `info = "arca/v1/<tipo>/<época>"` (época em decimal).
fn derive_key(ikm: &[u8], kind: ObjectKind, key_epoch: u32) -> Zeroizing<[u8; 32]> {
    let hkdf = Hkdf::<Sha256>::new(Some(HKDF_SALT), ikm);
    let info = format!("arca/v1/{}/{}", kind.as_str(), key_epoch);
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf.expand(info.as_bytes(), &mut okm[..])
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    okm
}

impl fmt::Debug for EpochKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EpochKeys")
            .field("epoch", &self.epoch)
            .field("keys", &"<redacted>")
            .finish()
    }
}

/// Um objeto cifrado, pronto para ir a um backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// SHA-256 dos bytes cifrados (ADR 0009).
    pub id: ObjectId,
    /// Cabeçalho (36 bytes) + texto cifrado + tag.
    pub bytes: Vec<u8>,
}

/// Cifra `plaintext` como objeto do tipo `kind`, com nonce aleatório do sistema operacional.
pub fn seal(keys: &EpochKeys, kind: ObjectKind, plaintext: &[u8]) -> Result<Sealed, Error> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|_| Error::Random)?;
    let bytes = seal_with_nonce(keys, kind, nonce, plaintext)?;
    Ok(Sealed {
        id: ObjectId::of(kind, &bytes),
        bytes,
    })
}

/// Decifra um objeto. `expected` é o tipo que o chamador espera (vem do `ObjectId`): um
/// backend que serve um manifest no lugar de um blob é recusado antes de decifrar.
pub fn open(
    keys: &EpochKeys,
    expected: ObjectKind,
    bytes: &[u8],
) -> Result<Zeroizing<Vec<u8>>, Error> {
    let (header, body) = Header::decode(bytes)?;
    if header.kind != expected {
        return Err(Error::KindMismatch);
    }
    if header.key_epoch != keys.epoch {
        return Err(Error::WrongKeyEpoch {
            found: header.key_epoch,
            expected: keys.epoch,
        });
    }
    if body.len() < TAG_LEN {
        return Err(Error::Decrypt);
    }
    let cipher = XChaCha20Poly1305::new(&Key::from(*keys.key_for(expected)));
    let plaintext = cipher
        .decrypt(
            &XNonce::from(header.nonce),
            Payload {
                msg: body,
                aad: &bytes[..HEADER_LEN],
            },
        )
        .map_err(|_| Error::Decrypt)?;
    Ok(Zeroizing::new(plaintext))
}

/// Cifra com nonce dado: cabeçalho de 36 bytes (também o AAD) + texto cifrado + tag.
/// Só `seal` e os testes de vetor chamam isto; um nonce repetido com a mesma chave quebra a cifra.
fn seal_with_nonce(
    keys: &EpochKeys,
    kind: ObjectKind,
    nonce: [u8; NONCE_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, Error> {
    let header = Header {
        format_version: FORMAT_VERSION as u16,
        alg: Alg::XChaCha20Poly1305,
        kind,
        key_epoch: keys.epoch,
        nonce,
    }
    .encode();
    let ciphertext = aead_encrypt(keys.key_for(kind), &nonce, &header, plaintext)?;
    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// XChaCha20-Poly1305 com AAD livre. Devolve texto cifrado + tag.
fn aead_encrypt(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, Error> {
    XChaCha20Poly1305::new(&Key::from(*key))
        .encrypt(
            &XNonce::from(*nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Encrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn test_vault_key() -> VaultKey {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        VaultKey::from_bytes(k)
    }

    fn keys(epoch: u32) -> EpochKeys {
        EpochKeys::derive(&test_vault_key(), epoch)
    }

    /// HKDF-SHA256 direto, para comparar com as chaves da arca.
    fn hkdf_32(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
        let mut out = [0u8; 32];
        hk.expand(info, &mut out).unwrap();
        out
    }

    /// PRNG xorshift de semente fixa, só para gerar entradas de teste.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    // 1
    #[test]
    fn round_trip_for_blob_and_manifest_at_edge_sizes() {
        let k = keys(1);
        for kind in [ObjectKind::Blob, ObjectKind::Manifest] {
            for n in [0usize, 1, 15, 16, 17, 1024 * 1024] {
                let pt = vec![0x5Au8; n];
                let sealed = seal(&k, kind, &pt).unwrap();
                assert_eq!(sealed.id, ObjectId::of(kind, &sealed.bytes));
                assert_eq!(sealed.bytes.len(), HEADER_LEN + n + TAG_LEN);
                let back = open(&k, kind, &sealed.bytes).unwrap();
                assert_eq!(&back[..], &pt[..], "kind {kind:?} size {n}");
            }
        }
    }

    // 2
    #[test]
    fn sealing_twice_gives_different_objects() {
        let k = keys(1);
        let a = seal(&k, ObjectKind::Blob, b"same text").unwrap();
        let b = seal(&k, ObjectKind::Blob, b"same text").unwrap();
        assert_ne!(a.bytes, b.bytes);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn a_thousand_seals_never_repeat_a_nonce() {
        let k = keys(1);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let s = seal(&k, ObjectKind::Blob, b"x").unwrap();
            let (h, _) = Header::decode(&s.bytes).unwrap();
            assert!(seen.insert(h.nonce), "nonce repeated");
        }
    }

    // 3
    #[test]
    fn flipping_any_single_byte_makes_open_fail() {
        let k = keys(1);
        let sealed = seal(&k, ObjectKind::Blob, b"a short secret").unwrap();
        for i in 0..sealed.bytes.len() {
            for mask in [0x01u8, 0x80, 0xFF] {
                let mut bad = sealed.bytes.clone();
                bad[i] ^= mask;
                assert!(
                    open(&k, ObjectKind::Blob, &bad).is_err(),
                    "byte {i} mask {mask:#x} was accepted"
                );
            }
        }
    }

    #[test]
    fn dropping_or_adding_bytes_makes_open_fail() {
        let k = keys(1);
        let sealed = seal(&k, ObjectKind::Blob, b"a short secret").unwrap();
        for len in 0..sealed.bytes.len() {
            assert!(open(&k, ObjectKind::Blob, &sealed.bytes[..len]).is_err());
        }
        let mut longer = sealed.bytes.clone();
        longer.push(0);
        assert_eq!(open(&k, ObjectKind::Blob, &longer), Err(Error::Decrypt));
    }

    // 4
    #[test]
    fn swapping_the_kind_in_the_header_is_rejected() {
        let k = keys(1);
        let sealed = seal(&k, ObjectKind::Blob, b"payload").unwrap();
        let mut swapped = sealed.bytes.clone();
        swapped[7] = ObjectKind::Manifest as u8;
        // o chamador esperava um blob
        assert_eq!(
            open(&k, ObjectKind::Blob, &swapped),
            Err(Error::KindMismatch)
        );
        // o chamador aceita manifest: a subchave e o AAD não batem
        assert_eq!(
            open(&k, ObjectKind::Manifest, &swapped),
            Err(Error::Decrypt)
        );
    }

    #[test]
    fn a_blob_does_not_open_as_a_manifest_and_vice_versa() {
        let k = keys(1);
        let blob = seal(&k, ObjectKind::Blob, b"p").unwrap();
        let manifest = seal(&k, ObjectKind::Manifest, b"p").unwrap();
        assert_eq!(
            open(&k, ObjectKind::Manifest, &blob.bytes),
            Err(Error::KindMismatch)
        );
        assert_eq!(
            open(&k, ObjectKind::Blob, &manifest.bytes),
            Err(Error::KindMismatch)
        );
    }

    #[test]
    fn changing_the_nonce_is_rejected() {
        let k = keys(1);
        let sealed = seal(&k, ObjectKind::Blob, b"payload").unwrap();
        let mut bad = sealed.bytes.clone();
        bad[12] ^= 1;
        assert_eq!(open(&k, ObjectKind::Blob, &bad), Err(Error::Decrypt));
    }

    // 5
    #[test]
    fn wrong_key_is_rejected() {
        let sealed = seal(&keys(1), ObjectKind::Blob, b"payload").unwrap();
        let other = EpochKeys::derive(&VaultKey::from_bytes([9u8; 32]), 1);
        assert_eq!(
            open(&other, ObjectKind::Blob, &sealed.bytes),
            Err(Error::Decrypt)
        );
    }

    #[test]
    fn wrong_epoch_is_rejected() {
        let sealed = seal(&keys(1), ObjectKind::Blob, b"payload").unwrap();
        assert_eq!(
            open(&keys(2), ObjectKind::Blob, &sealed.bytes),
            Err(Error::WrongKeyEpoch {
                found: 1,
                expected: 2
            })
        );
    }

    // 6
    #[test]
    fn body_shorter_than_a_tag_is_rejected_without_panic() {
        let k = keys(1);
        let sealed = seal(&k, ObjectKind::Blob, b"").unwrap();
        let header = &sealed.bytes[..HEADER_LEN];
        for body_len in 0..TAG_LEN {
            let mut bytes = header.to_vec();
            bytes.extend(std::iter::repeat_n(0u8, body_len));
            assert_eq!(open(&k, ObjectKind::Blob, &bytes), Err(Error::Decrypt));
        }
    }

    // 7
    #[test]
    fn epoch_keys_match_an_independent_hkdf_implementation() {
        // Calculado em Python (hmac/hashlib, RFC 5869) com ikm = bytes 0x00..0x1f,
        // salt "arca", info "arca/v1/<tipo>/<época>".
        let cases = [
            (
                0u32,
                "b085be706ec9201886fe58de820b3247b417ae27c7059460e85b601272a1e8fb",
                "40e1ca4f0ca1f84ed84ced88fa5814f72fb76baa41ecb17f3369fe7f12c28612",
            ),
            (
                1,
                "356c8d91876b2117e9e771e359ef8633044ce13672162ab47f4d777f14ad3f43",
                "ef584679bad7776892cd60c738376ca76ace62a30fa9fbe79530f2d39ce93092",
            ),
            (
                4294967295,
                "33758aadc79c6624f76897ad238ee99ce6ad6b08e601cf489c2d8eee77426d05",
                "db40bdb48e2f939b6b2f1f50284e862316c8d68719423b044f93c64c20110681",
            ),
        ];
        for (epoch, blob, manifest) in cases {
            let k = keys(epoch);
            assert_eq!(&k.blob[..], &hex(blob)[..], "blob epoch {epoch}");
            assert_eq!(
                &k.manifest[..],
                &hex(manifest)[..],
                "manifest epoch {epoch}"
            );
            assert_eq!(k.epoch(), epoch);
        }
    }

    // 8: draft-irtf-cfrg-xchacha-03, apêndice A.3.1
    #[test]
    fn xchacha20_poly1305_matches_the_ietf_draft_vector() {
        let pt = hex(
            "4c616469657320616e642047656e746c656d656e206f662074686520636c6173
                      73206f66202739393a204966204920636f756c64206f6666657220796f75206f
                      6e6c79206f6e652074697020666f7220746865206675747572652c2073756e73
                      637265656e20776f756c642062652069742e",
        );
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
            .try_into()
            .unwrap();
        let nonce: [u8; 24] = hex("404142434445464748494a4b4c4d4e4f5051525354555657")
            .try_into()
            .unwrap();
        let expected_ct = hex(
            "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb
                               731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452
                               2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9
                               21f9664c97637da9768812f615c68b13b52e",
        );
        let expected_tag = hex("c0875924c1c7987947deafd8780acf49");
        let out = aead_encrypt(&key, &nonce, &aad, &pt).unwrap();
        assert_eq!(&out[..expected_ct.len()], &expected_ct[..]);
        assert_eq!(&out[expected_ct.len()..], &expected_tag[..]);
    }

    // 9
    #[test]
    fn keys_are_separated_by_kind_epoch_and_vault_key() {
        let a = keys(1);
        assert_ne!(&a.blob[..], &a.manifest[..]);
        let b = keys(2);
        assert_ne!(&a.blob[..], &b.blob[..]);
        assert_ne!(&a.manifest[..], &b.manifest[..]);
        let c = EpochKeys::derive(&VaultKey::from_bytes([7u8; 32]), 1);
        assert_ne!(&a.blob[..], &c.blob[..]);
        assert_ne!(&a.manifest[..], &c.manifest[..]);
    }

    #[test]
    fn arca_keys_never_collide_with_other_warden_and_truthid_labels() {
        // Pares (salt, info) em uso hoje, copiados do código:
        //   warden-sync/bundle.rs, warden-sync/pairing/protocol.rs,
        //   warden-core/memory/cipher.rs, warden-truthid/crypto.rs, truthid/desktop/lib.rs.
        let others: [(&[u8], &[u8]); 7] = [
            (b"Warden Sync Bundle", b"bundle-content-key-v1"),
            (b"Warden Pairing", b"pairing-auth-v1"),
            (b"warden member vault", b"content"),
            (b"warden member vault", b"names"),
            (b"TruthID Pin Content", b"content-key-v1"),
            (b"TruthID", b""),
            (b"arca", b"content-key-v1"),
        ];
        let vk = [0x11u8; 32];
        let ours = EpochKeys::derive(&VaultKey::from_bytes(vk), 0);
        for (salt, info) in others {
            let theirs = hkdf_32(&vk, salt, info);
            assert_ne!(&ours.blob[..], &theirs[..], "{salt:?}/{info:?}");
            assert_ne!(&ours.manifest[..], &theirs[..], "{salt:?}/{info:?}");
        }
        // e os rótulos da arca não começam por nenhum prefixo de outro sistema
        for (salt, _) in &others[..6] {
            assert_ne!(*salt, HKDF_SALT);
        }
    }

    // 10
    #[test]
    fn debug_output_never_shows_key_or_plaintext() {
        let vk = VaultKey::from_bytes([0xABu8; 32]);
        let k = EpochKeys::derive(&vk, 3);
        let text = format!("{vk:?} {k:?}");
        assert!(!text.to_lowercase().contains("abab"));
        assert!(!text.contains("171"));
        assert!(text.contains("redacted"));

        let sealed = seal(&k, ObjectKind::Blob, b"TOP-SECRET-PLAINTEXT").unwrap();
        let errs = [
            format!(
                "{:?}",
                open(&k, ObjectKind::Blob, &sealed.bytes[..40]).unwrap_err()
            ),
            format!("{:?}", Error::Decrypt),
            format!("{:?}", Error::Random),
        ];
        for e in errs {
            assert!(!e.contains("TOP-SECRET"));
        }
    }

    // 11
    #[test]
    fn random_round_trips() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for i in 0..200u32 {
            let mut vk = [0u8; 32];
            vk.copy_from_slice(&rng.bytes(32));
            let k = EpochKeys::derive(&VaultKey::from_bytes(vk), i);
            let len = (rng.next() % 600) as usize;
            let pt = rng.bytes(len);
            let kind = if i % 2 == 0 {
                ObjectKind::Blob
            } else {
                ObjectKind::Manifest
            };
            let sealed = seal(&k, kind, &pt).unwrap();
            assert_eq!(&open(&k, kind, &sealed.bytes).unwrap()[..], &pt[..]);
        }
    }

    // determinismo do cabeçalho: o seal escreve o que a spec manda
    #[test]
    fn sealed_header_follows_the_spec() {
        let k = keys(7);
        let nonce = [0xC3u8; NONCE_LEN];
        let bytes = seal_with_nonce(&k, ObjectKind::Manifest, nonce, b"hi").unwrap();
        let (h, body) = Header::decode(&bytes).unwrap();
        assert_eq!(h.format_version as u32, FORMAT_VERSION);
        assert_eq!(h.alg, Alg::XChaCha20Poly1305);
        assert_eq!(h.kind, ObjectKind::Manifest);
        assert_eq!(h.key_epoch, 7);
        assert_eq!(h.nonce, nonce);
        assert_eq!(body.len(), 2 + TAG_LEN);
    }
}
