//! Frase de recuperação: as 24 palavras BIP-39 **são** a `vault_key` de 256 bits escrita em
//! palavras (ADR 0004). É só o codec do BIP-39: não há PBKDF2, senha nem `to_seed`.
//! O checksum de 8 bits pega erro de digitação; não prova autenticidade (quem decide se a
//! chave certa voltou é a tag AEAD ao abrir o primeiro objeto).

use crate::crypto::VaultKey;
use bip39::{Error as Bip39Error, Language, Mnemonic};
use std::fmt;
use zeroize::Zeroizing;

/// Número de palavras da frase da `vault_key` (256 bits + 8 de checksum = 24 × 11).
pub const PHRASE_WORDS: usize = 24;

/// A frase de 24 palavras. É a chave: sem `Display` (para não ir parar em log por `{}`),
/// `Debug` redigido e memória apagada ao sair de escopo.
pub struct RecoveryPhrase(Zeroizing<String>);

impl RecoveryPhrase {
    /// A frase com as palavras separadas por um espaço. Quem chama é dono do que fizer com ela.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn words(&self) -> impl Iterator<Item = &str> {
        self.0.split(' ')
    }
}

impl fmt::Debug for RecoveryPhrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryPhrase(<redacted>)")
    }
}

/// Por que uma frase digitada foi recusada. Nunca carrega a frase nem uma palavra dela.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PhraseError {
    /// Não são 24 palavras (a crate aceita 12 a 24, mas só 24 codificam a `vault_key`).
    #[error("recovery phrase must have {PHRASE_WORDS} words, found {found}")]
    WrongWordCount { found: usize },
    /// A palavra na posição `position` (0-based) não está na lista inglesa do BIP-39.
    #[error("word {} is not in the BIP-39 word list", .position + 1)]
    UnknownWord { position: usize },
    /// As palavras existem, mas o checksum não bate: troca, falta ou erro de digitação.
    #[error("recovery phrase checksum does not match")]
    BadChecksum,
    /// Qualquer outra recusa da biblioteca (não ocorre com inglês e 24 palavras).
    #[error("malformed recovery phrase")]
    Malformed,
}

impl VaultKey {
    /// Escreve a chave em 24 palavras. Mostre uma vez e não guarde (ADR 0004).
    pub fn to_phrase(&self) -> Result<RecoveryPhrase, PhraseError> {
        // 32 bytes sempre dão 24 palavras; o `map_err` existe para nunca haver pânico aqui.
        let mnemonic =
            Mnemonic::from_entropy(self.as_bytes()).map_err(|_| PhraseError::Malformed)?;
        // Capacidade para o pior caso (palavras de 8 letras): a `String` não realoca, então
        // não sobra cópia da frase no heap.
        let mut text = String::with_capacity(PHRASE_WORDS * 9);
        for (i, word) in mnemonic.words().enumerate() {
            if i > 0 {
                text.push(' ');
            }
            text.push_str(word);
        }
        Ok(RecoveryPhrase(Zeroizing::new(text)))
    }

    /// Lê as 24 palavras e devolve a chave. Aceita maiúsculas e qualquer espaço em branco
    /// entre as palavras (digitação humana); só inglês.
    pub fn from_phrase(phrase: &str) -> Result<VaultKey, PhraseError> {
        let found = phrase.split_whitespace().count();
        if found != PHRASE_WORDS {
            return Err(PhraseError::WrongWordCount { found });
        }
        // Forma canônica: palavras separadas por um espaço, minúsculas ASCII. Só encurta ou
        // mantém o tamanho (espaços colapsam, ASCII minúsculo não muda os bytes), então a
        // capacidade basta e não há realocação deixando cópia da frase para trás.
        let mut normalized = Zeroizing::new(String::with_capacity(phrase.len()));
        for (i, word) in phrase.split_whitespace().enumerate() {
            if i > 0 {
                normalized.push(' ');
            }
            normalized.extend(word.chars().map(|c| c.to_ascii_lowercase()));
        }
        let mnemonic =
            Mnemonic::parse_in_normalized(Language::English, &normalized).map_err(|e| match e {
                Bip39Error::BadWordCount(found) => PhraseError::WrongWordCount { found },
                Bip39Error::UnknownWord(position) => PhraseError::UnknownWord { position },
                Bip39Error::InvalidChecksum => PhraseError::BadChecksum,
                _ => PhraseError::Malformed,
            })?;

        let (entropy, len) = mnemonic.to_entropy_array();
        let entropy = Zeroizing::new(entropy);
        if len != 32 {
            return Err(PhraseError::Malformed);
        }
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&entropy[..32]);
        Ok(VaultKey::from_bytes(*key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::crypto::EpochKeys;
    use crate::disk::DiskStore;
    use crate::engine::{Engine, EngineError, Roots, Snapshot};
    use crate::error::Error;
    use crate::testutil::TempDir;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    // ---------- vetores oficiais ----------

    /// Os 8 vetores de 256 bits do `vectors.json` oficial (`trezor/python-mnemonic`), copiados
    /// do arquivo baixado em 2026-11-01 (entropia em hex, frase em inglês).
    const VECTORS: [(&str, &str); 8] = [
        (
            "0000000000000000000000000000000000000000000000000000000000000000",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art",
        ),
        (
            "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
            "legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth useful legal winner thank year wave sausage worth title",
        ),
        (
            "8080808080808080808080808080808080808080808080808080808080808080",
            "letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic avoid letter advice cage absurd amount doctor acoustic bless",
        ),
        (
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo vote",
        ),
        (
            "68a79eaca2324873eacc50cb9c6eca8cc68ea5d936f98787c60c7ebc74e6ce7c",
            "hamster diagram private dutch cause delay private meat slide toddler razor book happy fancy gospel tennis maple dilemma loan word shrug inflict delay length",
        ),
        (
            "9f6a2878b2520799a44ef18bc7df394e7061a224d2c33cd015b157d746869863",
            "panda eyebrow bullet gorilla call smoke muffin taste mesh discover soft ostrich alcohol speed nation flash devote level hobby quick inner drive ghost inside",
        ),
        (
            "066dca1a2bb7e8a1db2832148ce9933eea0f3ac9548d793112d9a95c9407efad",
            "all hour make first leader extend hole alien behind guard gospel lava path output census museum junior mass reopen famous sing advance salt reform",
        ),
        (
            "f585c11aec520db57dd353c69554b21a89b20fb0650966fa0a9d6f74fd989d8f",
            "void come effort suffer camp survey warrior heavy shoot primary clutch crush open amazing screen patrol group space point ten exist slush involve unfold",
        ),
    ];

    const ALL_ABANDON_ART: &str = VECTORS[0].1;

    fn key_from_hex(hex: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    /// A frase `phrase` com a palavra `position` trocada por `replacement`.
    fn with_word(phrase: &str, position: usize, replacement: &str) -> String {
        let mut words: Vec<&str> = phrase.split(' ').collect();
        words[position] = replacement;
        words.join(" ")
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn official_vectors_key_to_phrase() {
        for (hex, phrase) in VECTORS {
            let key = VaultKey::from_bytes(key_from_hex(hex));
            assert_eq!(key.to_phrase().unwrap().expose(), phrase, "{hex}");
        }
    }

    #[test]
    fn official_vectors_phrase_to_key() {
        for (hex, phrase) in VECTORS {
            let key = VaultKey::from_phrase(phrase).unwrap();
            assert_eq!(key.as_bytes(), &key_from_hex(hex), "{phrase}");
        }
    }

    // ---------- geração ----------

    #[test]
    fn generate_gives_a_nonzero_key_that_differs_every_time() {
        let a = VaultKey::generate().unwrap();
        let b = VaultKey::generate().unwrap();
        assert_ne!(a.as_bytes(), &[0u8; 32]);
        assert_ne!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn generated_keys_round_trip_through_the_phrase() {
        for round in 0..200 {
            let key = VaultKey::generate().unwrap();
            let phrase = key.to_phrase().unwrap();
            assert_eq!(phrase.words().count(), PHRASE_WORDS, "round {round}");
            let back = VaultKey::from_phrase(phrase.expose()).unwrap();
            assert_eq!(back.as_bytes(), key.as_bytes(), "round {round}");
        }
    }

    #[test]
    fn random_byte_keys_round_trip() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for _ in 0..200 {
            let mut bytes = [0u8; 32];
            for b in &mut bytes {
                *b = rng.next() as u8;
            }
            let key = VaultKey::from_bytes(bytes);
            let back = VaultKey::from_phrase(key.to_phrase().unwrap().expose()).unwrap();
            assert_eq!(back.as_bytes(), &bytes);
        }
    }

    // ---------- forma da frase ----------

    #[test]
    fn the_phrase_has_24_lowercase_words_separated_by_single_spaces() {
        let phrase = VaultKey::generate().unwrap().to_phrase().unwrap();
        let text = phrase.expose();
        assert_eq!(phrase.words().count(), 24);
        assert!(text.len() <= 24 * 9);
        assert!(!text.starts_with(' ') && !text.ends_with(' ') && !text.contains("  "));
        assert!(text.chars().all(|c| c.is_ascii_lowercase() || c == ' '));
    }

    // ---------- recusas ----------

    #[test]
    fn anything_but_24_words_is_a_word_count_error_even_if_the_checksum_is_valid() {
        // 12 palavras de um vetor oficial válido de 128 bits
        let twelve = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        assert_eq!(
            VaultKey::from_phrase(twelve).err(),
            Some(PhraseError::WrongWordCount { found: 12 })
        );
        let words: Vec<&str> = ALL_ABANDON_ART.split(' ').collect();
        let twenty_three = words[..23].join(" ");
        let twenty_five = format!("{ALL_ABANDON_ART} art");
        for (phrase, found) in [
            (twenty_three.as_str(), 23),
            (twenty_five.as_str(), 25),
            ("", 0),
            ("   \n\t ", 0),
            ("abandon", 1),
        ] {
            assert_eq!(
                VaultKey::from_phrase(phrase).err(),
                Some(PhraseError::WrongWordCount { found }),
                "{found} words"
            );
        }
    }

    #[test]
    fn a_misspelled_word_is_reported_by_position() {
        for position in [0usize, 7, 22, 23] {
            let phrase = with_word(ALL_ABANDON_ART, position, "abandom");
            assert_eq!(
                VaultKey::from_phrase(&phrase).err(),
                Some(PhraseError::UnknownWord { position }),
                "position {position}"
            );
        }
    }

    #[test]
    fn a_valid_word_in_the_wrong_place_is_a_checksum_error() {
        // Cada uma conferida contra um validador independente em Python (lista e SHA-256).
        let last_zoo = with_word(ALL_ABANDON_ART, 23, "zoo");
        let last_about = with_word(ALL_ABANDON_ART, 23, "about");
        let v2_last = with_word(VECTORS[1].1, 23, "abandon");
        let mut swapped: Vec<&str> = ALL_ABANDON_ART.split(' ').collect();
        swapped.swap(0, 23);
        let swapped = swapped.join(" ");
        for phrase in [last_zoo, last_about, v2_last, swapped] {
            assert_eq!(
                VaultKey::from_phrase(&phrase).err(),
                Some(PhraseError::BadChecksum),
                "{phrase}"
            );
        }
    }

    #[test]
    fn words_outside_the_english_list_are_refused() {
        for (replacement, label) in [
            ("abandón", "acento"),
            ("ábaco", "espanhol"),
            ("abandon-abandon", "hífen"),
            ("aban\0don", "NUL"),
            ("\u{feff}abandon", "BOM"),
            ("ａｂａｎｄｏｎ", "largura total"),
            ("abandon\u{200b}", "espaço de largura zero"),
        ] {
            let phrase = with_word(ALL_ABANDON_ART, 5, replacement);
            let err = VaultKey::from_phrase(&phrase).err();
            assert!(
                matches!(
                    err,
                    Some(PhraseError::UnknownWord { position: 5 })
                        | Some(PhraseError::WrongWordCount { .. })
                ),
                "{label}: {err:?}"
            );
        }
        // palavras separadas por hífen contam como uma só
        let hyphens = ALL_ABANDON_ART.replace(' ', "-");
        assert_eq!(
            VaultKey::from_phrase(&hyphens).err(),
            Some(PhraseError::WrongWordCount { found: 1 })
        );
    }

    // ---------- tolerância de digitação ----------

    #[test]
    fn human_typing_variations_are_accepted() {
        let canonical = VECTORS[1].1;
        let expected = key_from_hex(VECTORS[1].0);
        let upper = canonical.to_uppercase();
        let mixed: String = canonical
            .split(' ')
            .enumerate()
            .map(|(i, w)| {
                if i % 2 == 0 {
                    w.to_uppercase()
                } else {
                    w.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        for variant in [
            upper,
            mixed,
            format!("  {canonical}  "),
            format!("\n{canonical}\n"),
            canonical.replace(' ', "  "),
            canonical.replace(' ', "\t"),
            canonical.replace(' ', "\n"),
            canonical.replace(' ', "\u{a0}"),
            canonical.replace(' ', "\u{3000}"),
            canonical.replace(' ', " \r\n "),
        ] {
            let key =
                VaultKey::from_phrase(&variant).unwrap_or_else(|e| panic!("{variant:?}: {e}"));
            assert_eq!(key.as_bytes(), &expected, "{variant:?}");
        }
    }

    #[test]
    fn arbitrary_text_never_panics() {
        let mut rng = Rng(0xFEED_FACE_CAFE_BEEF);
        let alphabet: Vec<char> = "abandon zoo ÁéÇ\u{0}\u{feff}\u{a0}\t\n-ｚ\u{1F600}"
            .chars()
            .collect();
        for _ in 0..2000 {
            let n = (rng.next() % 200) as usize;
            let text: String = (0..n)
                .map(|_| alphabet[(rng.next() as usize) % alphabet.len()])
                .collect();
            let _ = VaultKey::from_phrase(&text);
        }
        // e variações de frases quase válidas
        for _ in 0..300 {
            let position = (rng.next() % 24) as usize;
            let words = ["abandon", "zoo", "art", "", "Zoo", "ZOO", "abandonn"];
            let w = words[(rng.next() as usize) % words.len()];
            let _ = VaultKey::from_phrase(&with_word(ALL_ABANDON_ART, position, w));
        }
    }

    // ---------- sigilo ----------

    #[test]
    fn debug_and_errors_never_show_the_phrase_or_its_words() {
        let key = VaultKey::from_bytes(key_from_hex(VECTORS[2].0));
        let phrase = key.to_phrase().unwrap();
        let shown = format!("{phrase:?}");
        assert_eq!(shown, "RecoveryPhrase(<redacted>)");
        for word in phrase.words() {
            assert!(!shown.contains(word), "{word}");
        }

        // erros: uma palavra inventada, uma frase quase certa e uma de contagem errada
        let typo = with_word(VECTORS[2].1, 3, "xyzzysegredo");
        let bad_sum = with_word(VECTORS[2].1, 23, "abandon");
        let short = VECTORS[2]
            .1
            .split(' ')
            .take(5)
            .collect::<Vec<_>>()
            .join(" ");
        for input in [typo, bad_sum, short] {
            let err = VaultKey::from_phrase(&input).unwrap_err();
            let text = format!("{err} {err:?}");
            assert!(
                !text.contains("xyzzy") && !text.contains("segredo"),
                "{text}"
            );
            for word in ["letter", "advice", "cage", "absurd", "bless", "acoustic"] {
                assert!(!text.contains(word), "{word} em {text}");
            }
        }
    }

    #[test]
    fn vault_key_debug_still_redacts() {
        let key = VaultKey::from_bytes(key_from_hex(VECTORS[0].0));
        assert_eq!(format!("{key:?}"), "VaultKey(<redacted>)");
    }

    // ---------- ponta a ponta da Fase 1 ----------

    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let rel = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path(), &rel, out);
                } else {
                    out.insert(rel, fs::read(entry.path()).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, "", &mut out);
        out
    }

    fn copy_dir(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).unwrap();
        for entry in fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let to = dst.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &to);
            } else {
                fs::copy(entry.path(), to).unwrap();
            }
        }
    }

    /// O critério da Fase 1 de ponta a ponta: perder a máquina e o único segredo que sobra são
    /// as 24 palavras anotadas, mais uma cópia do armazenamento.
    #[tokio::test]
    async fn the_vault_is_restored_on_a_clean_machine_from_the_24_words_alone() {
        // máquina A: chave nova, snapshot, frase anotada
        let a = TempDir::new();
        let vault = a.path().join("vault");
        fs::create_dir_all(vault.join("notas")).unwrap();
        fs::write(vault.join("notas/ação.md"), "olá\n".as_bytes()).unwrap();
        fs::write(vault.join("raiz.md"), b"conteudo").unwrap();
        let key_a = VaultKey::generate().unwrap();
        let store_a = Backend::Disk(DiskStore::open(a.path().join("store")).unwrap());
        let engine_a = Engine::new(store_a, EpochKeys::derive(&key_a, 1), "maquina-a").unwrap();
        let roots = Roots {
            vault: Some(vault.clone()),
            ..Roots::default()
        };
        let id = match engine_a.snapshot(&roots, 1).await.unwrap() {
            Snapshot::Created { id, .. } => id,
            other => panic!("{other:?}"),
        };
        let words = key_a.to_phrase().unwrap().expose().to_string();
        drop(key_a);

        // máquina B: só o armazenamento copiado e as palavras
        let b = TempDir::new();
        copy_dir(&a.path().join("store"), &b.path().join("store"));
        let key_b = VaultKey::from_phrase(&words).unwrap();
        let store_b = Backend::Disk(DiskStore::open(b.path().join("store")).unwrap());
        let engine_b = Engine::new(store_b, EpochKeys::derive(&key_b, 1), "maquina-b").unwrap();
        let target = b.path().join("vault");
        engine_b
            .restore(
                &id,
                &Roots {
                    vault: Some(target.clone()),
                    ..Roots::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(tree(&target), tree(&vault));
    }

    #[tokio::test]
    async fn a_different_valid_phrase_cannot_open_the_vault() {
        // Duas frases com checksum válido: a certa e outra qualquer. O checksum não distingue;
        // quem recusa é a tag AEAD.
        let a = TempDir::new();
        let vault = a.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        fs::write(vault.join("a.md"), b"1").unwrap();
        let key_a = VaultKey::from_phrase(VECTORS[4].1).unwrap();
        let store = Backend::Disk(DiskStore::open(a.path().join("store")).unwrap());
        let engine = Engine::new(store.clone(), EpochKeys::derive(&key_a, 1), "pc").unwrap();
        let roots = Roots {
            vault: Some(vault),
            ..Roots::default()
        };
        let id = match engine.snapshot(&roots, 1).await.unwrap() {
            Snapshot::Created { id, .. } => id,
            other => panic!("{other:?}"),
        };

        let other_key = VaultKey::from_phrase(VECTORS[5].1).unwrap();
        let wrong = Engine::new(store, EpochKeys::derive(&other_key, 1), "pc").unwrap();
        let result = wrong
            .restore(
                &id,
                &Roots {
                    vault: Some(a.path().join("destino")),
                    ..Roots::default()
                },
            )
            .await;
        assert_eq!(result, Err(EngineError::Object(Error::Decrypt)));
        assert!(!a.path().join("destino").exists());
    }
}
