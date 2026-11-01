//! Corpo do manifest (depois de decifrado): o estado completo do Vault num instante
//! (`SPEC-TECNICA.md` §5.3). Vem de fora, então só se constrói a partir de bytes por
//! `Manifest::from_slice`, que valida tudo.

use crate::crypto::{EpochKeys, Sealed};
use crate::error::Error;
use crate::object::{hex_digit, ObjectKind};
use crate::{open, seal, FORMAT_VERSION};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use unicode_normalization::is_nfc;
use zeroize::Zeroizing;

/// Teto de arquivos por manifest. Palpite a revisar na Fase 3/4.
pub const MAX_FILES: usize = 100_000;
/// Teto do JSON decifrado, em bytes. Palpite a revisar na Fase 3/4.
pub const MAX_MANIFEST_JSON_BYTES: usize = 64 * 1024 * 1024;
/// Teto do caminho inteiro, em bytes.
pub const MAX_PATH_BYTES: usize = 4096;
/// Teto de um componente do caminho, em bytes.
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Teto do nome do dispositivo, em bytes.
pub const MAX_DEVICE_BYTES: usize = 64;

/// Hash de 32 bytes, escrito no JSON como 64 hex minúsculos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash32(pub [u8; 32]);

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Hash32 {
    type Err = Error;

    /// Exatamente 64 dígitos hexadecimais minúsculos.
    fn from_str(s: &str) -> Result<Self, Error> {
        const BAD: Error = Error::InvalidManifest("invalid hash");
        let hex = s.as_bytes();
        if hex.len() != 64 {
            return Err(BAD);
        }
        let mut out = [0u8; 32];
        let (pairs, _) = hex.as_chunks::<2>();
        for (byte, [hi, lo]) in out.iter_mut().zip(pairs) {
            let digit = |c: u8| hex_digit(c).map_err(|_| BAD);
            *byte = (digit(*hi)? << 4) | digit(*lo)?;
        }
        Ok(Hash32(out))
    }
}

impl Serialize for Hash32 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hash32 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <&str>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Classe de uma entrada (ADR 0010).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Vault,
    Config,
    Secret,
}

/// Um arquivo do Vault no manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// Relativo à raiz da classe, separador `/`, NFC (ver `validate_path`).
    pub path: String,
    pub class: Class,
    /// Id (hash dos bytes cifrados) do blob com o conteúdo.
    pub blob: Hash32,
    /// Tamanho do texto claro, em bytes.
    pub size: u64,
    /// SHA-256 do texto claro (detecção de mudança local).
    pub sha256: Hash32,
}

/// Estado completo do Vault num instante.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u16,
    pub key_epoch: u32,
    /// Id do manifest que este dispositivo tinha como cabeça ao criar este (ADR 0005).
    #[serde(deserialize_with = "required_option")]
    pub base_version: Option<Hash32>,
    /// Só em manifest de fusão: as pontas incorporadas (ADR 0009). Vazio some do JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_from: Vec<Hash32>,
    pub counter: u64,
    pub device: String,
    /// Segundos Unix (UTC).
    pub created_at: u64,
    /// Em ordem estrita crescente de `path` (bytes).
    pub files: Vec<FileEntry>,
}

/// Valida um `path` de entrada. Os motivos são fixos e não repetem o caminho.
pub fn validate_path(path: &str) -> Result<(), Error> {
    if path.is_empty() {
        return Err(Error::InvalidPath("empty"));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(Error::InvalidPath("too long"));
    }
    // Controles (inclui NUL, quebra de linha, DEL e C1), `\` e os caracteres que o Windows proíbe.
    if path
        .chars()
        .any(|c| c.is_control() || matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err(Error::InvalidPath("forbidden character"));
    }
    if !is_nfc(path) {
        return Err(Error::InvalidPath("not in Unicode NFC"));
    }
    // `/` inicial ou final e `//` aparecem como componente vazio.
    for component in path.split('/') {
        validate_component(component)?;
    }
    Ok(())
}

fn validate_component(component: &str) -> Result<(), Error> {
    if component.is_empty() {
        return Err(Error::InvalidPath("empty component"));
    }
    if component == "." || component == ".." {
        return Err(Error::InvalidPath("dot component"));
    }
    if component.len() > MAX_COMPONENT_BYTES {
        return Err(Error::InvalidPath("component too long"));
    }
    if component.ends_with('.') || component.ends_with(' ') {
        return Err(Error::InvalidPath("component ends with dot or space"));
    }
    // O Windows trata `CON.txt` como `CON`: vale o trecho antes do primeiro ponto.
    let stem = component.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err(Error::InvalidPath("reserved name"));
    }
    Ok(())
}

/// Nome de dispositivo: vai parar em nome de arquivo (`.conflito-<dispositivo>-…`), então só
/// ASCII `[A-Za-z0-9._-]`, sem começar por ponto.
pub(crate) fn validate_device(device: &str) -> Result<(), Error> {
    let ok = !device.is_empty()
        && device.len() <= MAX_DEVICE_BYTES
        && !device.starts_with('.')
        && device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidManifest("invalid device name"))
    }
}

/// `base_version` sem `deserialize_with` viraria `None` se o campo faltasse; aqui ele é obrigatório.
fn required_option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Hash32>, D::Error> {
    Option::<Hash32>::deserialize(d)
}

impl Manifest {
    /// Interpreta o JSON decifrado: confere tamanho, JSON estrito (campo desconhecido,
    /// classe desconhecida, lixo no fim e tipos errados são erro) e todas as regras de `validate`.
    pub fn from_slice(bytes: &[u8]) -> Result<Manifest, Error> {
        if bytes.len() > MAX_MANIFEST_JSON_BYTES {
            return Err(Error::InvalidManifest("too large"));
        }
        // A mensagem do serde pode repetir trechos da entrada (nomes de arquivo): descartada.
        let manifest: Manifest = serde_json::from_slice(bytes)
            .map_err(|_| Error::InvalidManifest("malformed or unexpected JSON"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Regras que o JSON sozinho não garante: `format_version`, `device`, limite de arquivos,
    /// cada `path`, ordem estrita e unicidade (também sem diferença de caixa).
    pub fn validate(&self) -> Result<(), Error> {
        if u32::from(self.format_version) != FORMAT_VERSION {
            return Err(Error::UnknownFormatVersion(self.format_version));
        }
        validate_device(&self.device)?;
        if self.files.len() > MAX_FILES {
            return Err(Error::InvalidManifest("too many files"));
        }
        let mut folded = HashSet::with_capacity(self.files.len());
        for (i, file) in self.files.iter().enumerate() {
            validate_path(&file.path)?;
            if i > 0 && self.files[i - 1].path >= file.path {
                return Err(Error::InvalidManifest("files not in strict path order"));
            }
            if !folded.insert(file.path.to_lowercase()) {
                return Err(Error::InvalidManifest("paths differ only by case"));
            }
        }
        Ok(())
    }

    /// Valida e serializa em JSON compacto, com a ordem de campos fixa.
    pub fn to_vec(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| Error::InvalidManifest("serialization failed"))
    }

    /// Valida, confere que `key_epoch` é o das chaves e cifra como objeto `manifest`.
    pub fn seal(&self, keys: &EpochKeys) -> Result<Sealed, Error> {
        if self.key_epoch != keys.epoch() {
            return Err(Error::WrongKeyEpoch {
                found: self.key_epoch,
                expected: keys.epoch(),
            });
        }
        let json = Zeroizing::new(self.to_vec()?);
        seal(keys, ObjectKind::Manifest, &json)
    }

    /// Decifra como `manifest`, interpreta e confere que o `key_epoch` do corpo é o do cabeçalho.
    pub fn open(keys: &EpochKeys, bytes: &[u8]) -> Result<Manifest, Error> {
        let json = open(keys, ObjectKind::Manifest, bytes)?;
        let manifest = Manifest::from_slice(&json)?;
        // `open` já provou que o cabeçalho é da época de `keys`; o corpo só é texto cifrado
        // por quem tem a chave e pode divergir dele.
        if manifest.key_epoch != keys.epoch() {
            return Err(Error::WrongKeyEpoch {
                found: manifest.key_epoch,
                expected: keys.epoch(),
            });
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{open, seal, VaultKey};
    use crate::envelope::{Header, HEADER_LEN};
    use crate::object::{ObjectId, ObjectKind};
    use crate::FORMAT_VERSION;

    // ---------- ajudantes ----------

    fn keys(epoch: u32) -> EpochKeys {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        EpochKeys::derive(&VaultKey::from_bytes(k), epoch)
    }

    fn h(byte: u8) -> Hash32 {
        Hash32([byte; 32])
    }

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            class: Class::Vault,
            blob: h(0xAA),
            size: 10,
            sha256: h(0xBB),
        }
    }

    fn manifest(epoch: u32, paths: &[&str]) -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: epoch,
            base_version: None,
            merged_from: vec![],
            counter: 1,
            device: "portatil-fabio".to_string(),
            created_at: 1_793_455_331,
            files: paths.iter().map(|p| entry(p)).collect(),
        }
    }

    /// JSON de um manifest válido, para os testes que o estragam de propósito.
    fn valid_json() -> String {
        let m = manifest(3, &["a.md"]);
        String::from_utf8(m.to_vec().unwrap()).unwrap()
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
        fn hash(&mut self) -> Hash32 {
            let mut out = [0u8; 32];
            for b in &mut out {
                *b = self.next() as u8;
            }
            Hash32(out)
        }
    }

    /// Caminho válido (componentes de até 254 bytes) com exatamente `n` bytes.
    fn path_of_len(n: usize) -> String {
        let mut s = String::new();
        while s.len() < n {
            if !s.is_empty() {
                s.push('/');
            }
            let take = (n - s.len()).min(254);
            s.push_str(&"a".repeat(take));
        }
        assert_eq!(s.len(), n);
        s
    }

    const HEX_AA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEX_BB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // ---------- Hash32 ----------

    #[test]
    fn hash32_displays_as_64_lowercase_hex_and_parses_back() {
        assert_eq!(h(0xAA).to_string(), HEX_AA);
        assert_eq!(HEX_AA.parse::<Hash32>(), Ok(h(0xAA)));
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(9);
        }
        let x = Hash32(bytes);
        assert_eq!(x.to_string().parse::<Hash32>(), Ok(x));
    }

    #[test]
    fn hash32_rejects_malformed_strings() {
        let upper = HEX_AA.to_uppercase();
        let short = &HEX_AA[..63];
        let long = format!("{HEX_AA}a");
        let non_hex = HEX_AA.replacen('a', "g", 1);
        let bad = [
            String::new(),
            upper,
            short.to_string(),
            long,
            non_hex,
            format!(" {HEX_AA}"),
            format!("{HEX_AA}\n"),
            format!("0x{}", &HEX_AA[2..]),
            "é".repeat(32),
        ];
        for s in bad {
            assert!(s.parse::<Hash32>().is_err(), "should reject {s:?}");
        }
    }

    // ---------- validate_path ----------

    #[test]
    fn valid_paths_are_accepted() {
        let at_limit_component = "a".repeat(MAX_COMPONENT_BYTES);
        let at_limit_path = path_of_len(MAX_PATH_BYTES);
        for p in [
            "a.md",
            "memoria/projeto-x.md",
            "notas/ação.md", // NFC
            ".obsidian/app.json",
            "config.toml",
            "a b/c d.md",
            "console.md", // contém "con", mas não é o nome reservado
            "COM10.md",   // só COM1 a COM9 são reservados
            at_limit_component.as_str(),
            at_limit_path.as_str(),
        ] {
            assert_eq!(validate_path(p), Ok(()), "should accept {p:?}");
        }
    }

    #[test]
    fn structurally_bad_paths_are_rejected() {
        for p in [
            "", "/a", "a/", "/", "a//b", "./a", "a/./b", "a/.", ".", "..", "../a", "a/../b",
            "a/..", "a\\b", "C:/x", "a/b:c",
        ] {
            assert!(
                matches!(validate_path(p), Err(Error::InvalidPath(_))),
                "should reject {p:?}"
            );
        }
    }

    #[test]
    fn control_and_forbidden_characters_are_rejected() {
        for p in [
            "a\0b", "a\nb", "a\rb", "a\tb", "a\u{7f}b", "a\u{1b}b", "a<b", "a>b", "a\"b", "a|b",
            "a?b", "a*b",
        ] {
            assert!(
                matches!(validate_path(p), Err(Error::InvalidPath(_))),
                "should reject {p:?}"
            );
        }
    }

    #[test]
    fn non_nfc_paths_are_rejected() {
        let nfc = "caf\u{e9}.md";
        let nfd = "cafe\u{301}.md";
        assert_eq!(validate_path(nfc), Ok(()));
        assert!(matches!(validate_path(nfd), Err(Error::InvalidPath(_))));
        // hangul decomposto (jamo) também não é NFC
        assert!(matches!(
            validate_path("\u{1112}\u{1161}\u{11ab}.md"),
            Err(Error::InvalidPath(_))
        ));
    }

    #[test]
    fn length_limits_are_enforced() {
        let long_component = "a".repeat(MAX_COMPONENT_BYTES + 1);
        // 2 bytes por caractere: o limite é em bytes, não em caracteres
        let long_by_bytes = "é".repeat(MAX_COMPONENT_BYTES / 2 + 1);
        let long_path = path_of_len(MAX_PATH_BYTES + 1);
        assert_eq!(long_path.len(), MAX_PATH_BYTES + 1);
        for p in [long_component, long_by_bytes, long_path] {
            assert!(
                matches!(validate_path(&p), Err(Error::InvalidPath(_))),
                "should reject a path of {} bytes",
                p.len()
            );
        }
    }

    #[test]
    fn windows_reserved_names_are_rejected() {
        for p in [
            "CON",
            "con",
            "Con.txt",
            "dir/NUL.md",
            "PRN",
            "AUX.x",
            "COM1",
            "com9.md",
            "LPT1",
            "lpt9.txt",
            "a/CON/b",
        ] {
            assert!(
                matches!(validate_path(p), Err(Error::InvalidPath(_))),
                "should reject {p:?}"
            );
        }
    }

    #[test]
    fn components_ending_in_dot_or_space_are_rejected() {
        for p in ["a.", "a ", "dir./x", "dir /x", "x/y."] {
            assert!(
                matches!(validate_path(p), Err(Error::InvalidPath(_))),
                "should reject {p:?}"
            );
        }
    }

    #[test]
    fn path_error_never_repeats_the_path() {
        let err = validate_path("segredo/../nome-secreto.md").unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(!text.contains("segredo") && !text.contains("nome-secreto"));
    }

    // ---------- serialização ----------

    /// Vetor fixo: o JSON canônico é contrato de formato (é o que vai cifrado).
    #[test]
    fn golden_json() {
        let m = Manifest {
            format_version: 1,
            key_epoch: 3,
            base_version: None,
            merged_from: vec![],
            counter: 17,
            device: "portatil-fabio".to_string(),
            created_at: 1_793_455_331, // 2026-10-31T14:02:11Z
            files: vec![FileEntry {
                path: "memoria/projeto-x.md".to_string(),
                class: Class::Vault,
                blob: h(0xAA),
                size: 2048,
                sha256: h(0xBB),
            }],
        };
        let expected = format!(
            concat!(
                r#"{{"format_version":1,"key_epoch":3,"base_version":null,"counter":17,"#,
                r#""device":"portatil-fabio","created_at":1793455331,"files":["#,
                r#"{{"path":"memoria/projeto-x.md","class":"vault","blob":"{aa}","#,
                r#""size":2048,"sha256":"{bb}"}}]}}"#
            ),
            aa = HEX_AA,
            bb = HEX_BB
        );
        assert_eq!(String::from_utf8(m.to_vec().unwrap()).unwrap(), expected);
        assert_eq!(Manifest::from_slice(expected.as_bytes()), Ok(m));
    }

    #[test]
    fn merged_from_appears_only_when_not_empty() {
        let mut m = manifest(1, &["a.md"]);
        assert!(!valid_json().contains("merged_from"));
        m.base_version = Some(h(1));
        m.merged_from = vec![h(1), h(2)];
        let json = String::from_utf8(m.to_vec().unwrap()).unwrap();
        assert!(json.contains(&format!(r#""merged_from":["{}","{}"]"#, h(1), h(2))));
        assert_eq!(Manifest::from_slice(json.as_bytes()), Ok(m));
    }

    #[test]
    fn empty_vault_and_all_classes_round_trip() {
        let empty = manifest(0, &[]);
        assert_eq!(Manifest::from_slice(&empty.to_vec().unwrap()), Ok(empty));

        let mut m = manifest(u32::MAX, &["a.md", "b.toml", "c/d"]);
        m.files[1].class = Class::Config;
        m.files[2].class = Class::Secret;
        m.files[0].size = u64::MAX;
        m.counter = u64::MAX;
        m.created_at = u64::MAX;
        m.base_version = Some(h(9));
        assert_eq!(Manifest::from_slice(&m.to_vec().unwrap()), Ok(m));
    }

    #[test]
    fn to_vec_refuses_an_invalid_manifest() {
        let mut m = manifest(1, &["b.md", "a.md"]); // fora de ordem
        assert!(matches!(m.to_vec(), Err(Error::InvalidManifest(_))));
        m.files = vec![entry("../x")];
        assert!(matches!(m.to_vec(), Err(Error::InvalidPath(_))));
    }

    // ---------- from_slice: entrada hostil ----------

    #[test]
    fn unknown_format_version_is_rejected() {
        for v in [0, 2, 3, 255] {
            let json =
                valid_json().replace(r#""format_version":1"#, &format!(r#""format_version":{v}"#));
            assert_eq!(
                Manifest::from_slice(json.as_bytes()),
                Err(Error::UnknownFormatVersion(v)),
                "version {v}"
            );
        }
    }

    #[test]
    fn unknown_fields_are_rejected_at_both_levels() {
        let top = valid_json().replacen('{', r#"{"extra":1,"#, 1);
        assert!(Manifest::from_slice(top.as_bytes()).is_err());
        let nested = valid_json().replace(r#""class":"vault""#, r#""class":"vault","extra":1"#);
        assert!(Manifest::from_slice(nested.as_bytes()).is_err());
    }

    #[test]
    fn unknown_class_is_rejected() {
        for c in ["Vault", "VAULT", "secrets", "", "other"] {
            let json = valid_json().replace(r#""class":"vault""#, &format!(r#""class":"{c}""#));
            assert!(
                matches!(
                    Manifest::from_slice(json.as_bytes()),
                    Err(Error::InvalidManifest(_))
                ),
                "class {c:?}"
            );
        }
    }

    #[test]
    fn missing_fields_are_rejected() {
        for field in [
            "format_version",
            "key_epoch",
            "base_version",
            "counter",
            "device",
            "created_at",
            "files",
        ] {
            let mut v: serde_json::Value = serde_json::from_str(&valid_json()).unwrap();
            v.as_object_mut().unwrap().remove(field);
            let bytes = serde_json::to_vec(&v).unwrap();
            assert!(Manifest::from_slice(&bytes).is_err(), "without {field}");
        }
        for field in ["path", "class", "blob", "size", "sha256"] {
            let mut v: serde_json::Value = serde_json::from_str(&valid_json()).unwrap();
            v["files"][0].as_object_mut().unwrap().remove(field);
            let bytes = serde_json::to_vec(&v).unwrap();
            assert!(
                Manifest::from_slice(&bytes).is_err(),
                "entry without {field}"
            );
        }
    }

    #[test]
    fn wrong_types_and_ranges_are_rejected() {
        let cases = [
            (r#""key_epoch":3"#, r#""key_epoch":-1"#),
            (r#""key_epoch":3"#, r#""key_epoch":4294967296"#),
            (r#""key_epoch":3"#, r#""key_epoch":3.5"#),
            (r#""key_epoch":3"#, r#""key_epoch":"3""#),
            (r#""counter":1"#, r#""counter":-1"#),
            (r#""counter":1"#, r#""counter":18446744073709551616"#),
            (r#""size":10"#, r#""size":-1"#),
            (r#""size":10"#, r#""size":1e3"#),
            (r#""base_version":null"#, r#""base_version":"curto""#),
            (r#""base_version":null"#, r#""base_version":1"#),
            (r#""files":["#, r#""files":{"#),
            (r#""format_version":1"#, r#""format_version":65536"#),
        ];
        for (from, to) in cases {
            let json = valid_json().replace(from, to);
            assert_ne!(json, valid_json(), "replacement missed: {from}");
            assert!(Manifest::from_slice(json.as_bytes()).is_err(), "{to}");
        }
    }

    #[test]
    fn malformed_hashes_are_rejected() {
        let upper = HEX_AA.to_uppercase();
        for bad in [&HEX_AA[..63], upper.as_str(), "", "zz"] {
            let json = valid_json().replacen(HEX_AA, bad, 1);
            assert!(
                Manifest::from_slice(json.as_bytes()).is_err(),
                "hash {bad:?}"
            );
        }
        let json = valid_json().replacen(
            r#""base_version":null"#,
            r#""merged_from":["x"],"base_version":null"#,
            1,
        );
        assert!(Manifest::from_slice(json.as_bytes()).is_err());
    }

    #[test]
    fn empty_garbage_trailing_and_non_utf8_input_is_rejected() {
        let valid = valid_json();
        let trailing = format!("{valid}x");
        let two = format!("{valid}{valid}");
        let bad: [&[u8]; 7] = [
            b"",
            b"null",
            b"[]",
            b"{}",
            trailing.as_bytes(),
            two.as_bytes(),
            b"\xff\xfe\x00",
        ];
        for input in bad {
            assert!(Manifest::from_slice(input).is_err(), "{input:?}");
        }
        // BOM no começo: JSON estrito não aceita
        let bom = format!("\u{feff}{valid}");
        assert!(Manifest::from_slice(bom.as_bytes()).is_err());
    }

    #[test]
    fn truncation_at_every_length_is_an_error_not_a_panic() {
        let json = valid_json().into_bytes();
        for len in 0..json.len() {
            assert!(Manifest::from_slice(&json[..len]).is_err(), "length {len}");
        }
        assert!(Manifest::from_slice(&json).is_ok());
    }

    #[test]
    fn oversized_input_is_rejected_before_parsing() {
        let valid = valid_json();
        // JSON válido com espaços até passar do teto: só o tamanho o torna inválido
        let mut big = valid.into_bytes();
        big.resize(MAX_MANIFEST_JSON_BYTES + 1, b' ');
        assert!(matches!(
            Manifest::from_slice(&big),
            Err(Error::InvalidManifest(_))
        ));
        big.truncate(MAX_MANIFEST_JSON_BYTES);
        assert!(Manifest::from_slice(&big).is_ok());
    }

    #[test]
    fn parse_error_never_repeats_input() {
        let json = valid_json().replacen('{', r#"{"SEGREDO-NOME-DE-ARQUIVO":1,"#, 1);
        let err = Manifest::from_slice(json.as_bytes()).unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(!text.contains("SEGREDO"), "{text}");
    }

    #[test]
    fn an_invalid_path_inside_the_json_is_rejected() {
        for p in ["../x", "/abs", "a//b", "", "CON", "cafe\\u0301.md"] {
            let json = valid_json().replace(r#""path":"a.md""#, &format!(r#""path":"{p}""#));
            assert!(Manifest::from_slice(json.as_bytes()).is_err(), "path {p:?}");
        }
    }

    // ---------- validate: dispositivo, ordem, unicidade, limite ----------

    #[test]
    fn device_name_rules() {
        let at_limit = "d".repeat(MAX_DEVICE_BYTES);
        for ok in ["portatil-fabio", "a", "pc_1.local", at_limit.as_str()] {
            let mut m = manifest(1, &[]);
            m.device = ok.to_string();
            assert_eq!(m.validate(), Ok(()), "{ok:?}");
        }
        let too_long = "d".repeat(MAX_DEVICE_BYTES + 1);
        for bad in [
            "",
            ".oculto",
            "a b",
            "a/b",
            "a\\b",
            "ação",
            "a\nb",
            "a\0b",
            too_long.as_str(),
        ] {
            let mut m = manifest(1, &[]);
            m.device = bad.to_string();
            assert!(
                matches!(m.validate(), Err(Error::InvalidManifest(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn files_must_be_in_strictly_increasing_path_order() {
        assert_eq!(manifest(1, &["a.md", "b.md", "c/d.md"]).validate(), Ok(()));
        for paths in [
            &["b.md", "a.md"][..],
            &["a.md", "a.md"], // duplicata exata
            &["a.md", "c.md", "b.md"],
        ] {
            assert!(
                matches!(
                    manifest(1, paths).validate(),
                    Err(Error::InvalidManifest(_))
                ),
                "{paths:?}"
            );
        }
    }

    /// Ordem por bytes: maiúsculas vêm antes de minúsculas. Não é ordem "natural".
    #[test]
    fn order_is_by_bytes_not_by_locale() {
        assert_eq!(manifest(1, &["B.md", "a.md"]).validate(), Ok(()));
        assert!(manifest(1, &["a.md", "B.md"]).validate().is_err());
    }

    #[test]
    fn paths_differing_only_by_case_are_rejected_as_duplicates() {
        // Em ordem de bytes e distintos, mas colidem em macOS e Windows
        assert!(matches!(
            manifest(1, &["A.md", "a.md"]).validate(),
            Err(Error::InvalidManifest(_))
        ));
        assert!(matches!(
            manifest(1, &["Notas/x.md", "notas/x.md"]).validate(),
            Err(Error::InvalidManifest(_))
        ));
    }

    #[test]
    fn same_path_in_different_classes_is_still_a_duplicate() {
        let mut m = manifest(1, &["a.md", "a.md"]);
        m.files[1].class = Class::Config;
        assert!(m.validate().is_err());
    }

    #[test]
    fn file_count_limit_is_enforced() {
        let paths: Vec<String> = (0..=MAX_FILES).map(|i| format!("{i:06}.md")).collect();
        let mut m = manifest(1, &[]);
        m.files = paths[..MAX_FILES].iter().map(|p| entry(p)).collect();
        assert_eq!(m.validate(), Ok(()));
        m.files.push(entry(&paths[MAX_FILES]));
        assert!(matches!(m.validate(), Err(Error::InvalidManifest(_))));
        // e a leitura de um JSON desse tamanho também recusa
        let json = serde_json::to_vec(&m).unwrap();
        assert!(json.len() < MAX_MANIFEST_JSON_BYTES);
        assert!(matches!(
            Manifest::from_slice(&json),
            Err(Error::InvalidManifest(_))
        ));
    }

    #[test]
    fn wrong_format_version_in_a_value_is_refused_by_validate() {
        let mut m = manifest(1, &[]);
        m.format_version = 2;
        assert_eq!(m.validate(), Err(Error::UnknownFormatVersion(2)));
    }

    // ---------- seal / open ----------

    #[test]
    fn seal_then_open_round_trips() {
        let m = manifest(3, &["a.md", "b/c.md"]);
        let sealed = m.seal(&keys(3)).unwrap();
        assert_eq!(sealed.id, ObjectId::of(ObjectKind::Manifest, &sealed.bytes));
        assert_eq!(sealed.id.kind, ObjectKind::Manifest);
        let (header, _) = Header::decode(&sealed.bytes).unwrap();
        assert_eq!(header.kind, ObjectKind::Manifest);
        assert_eq!(header.key_epoch, 3);
        assert_eq!(Manifest::open(&keys(3), &sealed.bytes), Ok(m));
    }

    #[test]
    fn sealed_bytes_do_not_contain_paths_or_device() {
        let m = manifest(3, &["segredo/projeto-x.md"]);
        let sealed = m.seal(&keys(3)).unwrap();
        let hay = &sealed.bytes;
        for needle in [&b"segredo"[..], b"projeto-x", b"portatil-fabio"] {
            assert!(!hay.windows(needle.len()).any(|w| w == needle));
        }
    }

    #[test]
    fn sealing_the_same_manifest_twice_gives_different_objects() {
        let m = manifest(3, &["a.md"]);
        let a = m.seal(&keys(3)).unwrap();
        let b = m.seal(&keys(3)).unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn seal_refuses_a_manifest_from_another_epoch_than_the_keys() {
        let m = manifest(5, &["a.md"]);
        assert_eq!(
            m.seal(&keys(3)),
            Err(Error::WrongKeyEpoch {
                found: 5,
                expected: 3
            })
        );
    }

    #[test]
    fn seal_refuses_an_invalid_manifest() {
        let m = manifest(3, &["b.md", "a.md"]);
        assert!(matches!(m.seal(&keys(3)), Err(Error::InvalidManifest(_))));
    }

    #[test]
    fn open_refuses_a_body_whose_epoch_differs_from_the_header() {
        // Quem tem a chave da época 3 cifra um corpo que diz "época 5". A tag confere
        // (o cabeçalho é íntegro), mas o corpo mente: tem de ser recusado.
        let forged = manifest(5, &["a.md"]).to_vec().unwrap();
        let sealed = seal(&keys(3), ObjectKind::Manifest, &forged).unwrap();
        assert_eq!(
            Manifest::open(&keys(3), &sealed.bytes),
            Err(Error::WrongKeyEpoch {
                found: 5,
                expected: 3
            })
        );
    }

    #[test]
    fn open_refuses_a_valid_envelope_with_an_invalid_body() {
        for body in [&b"not json"[..], b"{}", b""] {
            let sealed = seal(&keys(3), ObjectKind::Manifest, body).unwrap();
            assert!(
                matches!(
                    Manifest::open(&keys(3), &sealed.bytes),
                    Err(Error::InvalidManifest(_))
                ),
                "{body:?}"
            );
        }
    }

    #[test]
    fn open_refuses_wrong_kind_wrong_epoch_wrong_key_and_tampering() {
        let m = manifest(3, &["a.md"]);
        let sealed = m.seal(&keys(3)).unwrap();

        let blob = seal(&keys(3), ObjectKind::Blob, &m.to_vec().unwrap()).unwrap();
        assert_eq!(
            Manifest::open(&keys(3), &blob.bytes),
            Err(Error::KindMismatch)
        );

        assert_eq!(
            Manifest::open(&keys(4), &sealed.bytes),
            Err(Error::WrongKeyEpoch {
                found: 3,
                expected: 4
            })
        );

        let other = EpochKeys::derive(&VaultKey::from_bytes([9u8; 32]), 3);
        assert_eq!(Manifest::open(&other, &sealed.bytes), Err(Error::Decrypt));

        for i in [0, HEADER_LEN, HEADER_LEN + 5, sealed.bytes.len() - 1] {
            let mut bad = sealed.bytes.clone();
            bad[i] ^= 0x01;
            assert!(Manifest::open(&keys(3), &bad).is_err(), "byte {i}");
        }
    }

    #[test]
    fn what_open_returns_is_what_blob_open_decrypts_to() {
        let m = manifest(3, &["a.md"]);
        let sealed = m.seal(&keys(3)).unwrap();
        let plain = open(&keys(3), ObjectKind::Manifest, &sealed.bytes).unwrap();
        assert_eq!(plain.as_slice(), m.to_vec().unwrap().as_slice());
    }

    // ---------- propriedades ----------

    #[test]
    fn random_manifests_round_trip_through_json_and_seal() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..200u32 {
            let n = (rng.next() % 20) as usize;
            let epoch = (rng.next() % 5) as u32;
            let mut m = manifest(epoch, &[]);
            m.counter = rng.next();
            m.created_at = rng.next();
            m.base_version = rng.next().is_multiple_of(2).then(|| rng.hash());
            if rng.next().is_multiple_of(4) {
                m.merged_from = vec![rng.hash(), rng.hash()];
            }
            for i in 0..n {
                m.files.push(FileEntry {
                    path: format!(
                        "{i:05}-{:x}/n\u{e3}o-{:x}.md",
                        rng.next() % 1000,
                        rng.next() % 1000
                    ),
                    class: [Class::Vault, Class::Config, Class::Secret][(rng.next() % 3) as usize],
                    blob: rng.hash(),
                    size: rng.next(),
                    sha256: rng.hash(),
                });
            }
            assert_eq!(m.validate(), Ok(()), "round {round}");
            let json = m.to_vec().unwrap();
            assert_eq!(
                Manifest::from_slice(&json).as_ref(),
                Ok(&m),
                "round {round}"
            );
            assert_eq!(Manifest::from_slice(&json).unwrap().to_vec().unwrap(), json);
            let sealed = m.seal(&keys(epoch)).unwrap();
            assert_eq!(Manifest::open(&keys(epoch), &sealed.bytes), Ok(m));
        }
    }

    #[test]
    fn random_and_mutated_bytes_never_panic_and_survivors_are_valid() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let base = manifest(3, &["a.md", "b/c.md"]).to_vec().unwrap();
        for round in 0..2000u32 {
            let input: Vec<u8> = if round % 2 == 0 {
                let n = (rng.next() % 300) as usize;
                (0..n).map(|_| rng.next() as u8).collect()
            } else {
                let mut b = base.clone();
                for _ in 0..(1 + rng.next() % 3) {
                    let i = (rng.next() as usize) % b.len();
                    b[i] = rng.next() as u8;
                }
                b
            };
            if let Ok(m) = Manifest::from_slice(&input) {
                assert_eq!(m.validate(), Ok(()), "round {round}");
                assert_eq!(Manifest::from_slice(&m.to_vec().unwrap()), Ok(m));
            }
        }
    }
}
