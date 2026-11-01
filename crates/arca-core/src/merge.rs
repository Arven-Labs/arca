//! Fusão de duas pontas (Fase 2, `SPEC-TECNICA.md` §7): ancestral comum no grafo de manifests
//! e regra de conflito por arquivo, em três vias. Funções puras: não leem nem gravam nada.

use crate::error::Error;
use crate::manifest::{validate_path, Class, FileEntry, Manifest};
use crate::object::ObjectId;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Ancestral comum mais próximo de `a` e `b` no grafo (`base_version` e `merged_from`).
/// Cada manifest conta como ancestral de si mesmo. Entre vários candidatos, só valem os
/// maximais (nenhum é ancestral de outro candidato); empate: maior `(counter, hash)`.
/// `None` se não há ancestral comum na lista (raízes distintas ou histórico parcial).
pub fn common_ancestor(
    all: &[(ObjectId, Manifest)],
    a: &ObjectId,
    b: &ObjectId,
) -> Option<ObjectId> {
    let graph: HashMap<[u8; 32], &(ObjectId, Manifest)> =
        all.iter().map(|entry| (entry.0.hash, entry)).collect();
    let reach_a = reachable(&graph, a.hash);
    let reach_b = reachable(&graph, b.hash);
    // só pode ser devolvido quem está na lista (pais ausentes são histórico parcial)
    let common: Vec<[u8; 32]> = reach_a
        .intersection(&reach_b)
        .filter(|hash| graph.contains_key(*hash))
        .copied()
        .collect();
    // maximais: ninguém mais, entre os comuns, os tem como ancestral próprio
    let maximal = common.iter().filter(|candidate| {
        !common
            .iter()
            .any(|other| other != *candidate && reachable(&graph, *other).contains(*candidate))
    });
    maximal
        .filter_map(|hash| graph.get(hash))
        .max_by_key(|(id, manifest)| (manifest.counter, id.hash))
        .map(|(id, _)| *id)
}

/// Hashes alcançáveis a partir de `from` (ele mesmo incluso) por `base_version`/`merged_from`.
fn reachable(
    graph: &HashMap<[u8; 32], &(ObjectId, Manifest)>,
    from: [u8; 32],
) -> HashSet<[u8; 32]> {
    let mut seen = HashSet::new();
    let mut stack = vec![from];
    while let Some(hash) = stack.pop() {
        if !seen.insert(hash) {
            continue;
        }
        if let Some((_, manifest)) = graph.get(&hash) {
            stack.extend(
                manifest
                    .base_version
                    .iter()
                    .chain(&manifest.merged_from)
                    .map(|parent| parent.0),
            );
        }
    }
    seen
}

/// Uma ponta que entra na fusão.
#[derive(Debug, Clone, Copy)]
pub struct Side<'a> {
    pub id: ObjectId,
    pub manifest: &'a Manifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    /// Mudou nos dois lados, com conteúdos diferentes (inclui criado nos dois).
    BothEdited,
    /// Removido de um lado e editado do outro: a edição fica.
    DeletedVsEdited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub class: Class,
    pub path: String,
    pub kind: ConflictKind,
    /// A entrada que perdeu o lugar; `None` quando o perdedor é uma remoção.
    pub loser: Option<FileEntry>,
    /// Dispositivo do manifest de onde veio o perdedor.
    pub loser_device: String,
    /// Só na classe `vault`: caminho da cópia `.conflito-*` que entrou nos `files`.
    pub copy_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// Estado fundido, em ordem estrita `(classe, path)`, já com as cópias `.conflito-*`.
    pub files: Vec<FileEntry>,
    pub conflicts: Vec<Conflict>,
}

/// Regra de decisão por arquivo da spec §7, comparando o `sha256` do texto claro. É simétrica:
/// `merge_files(b, x, y)` e `merge_files(b, y, x)` dão o mesmo resultado.
pub fn merge_files(base: &[FileEntry], left: Side, right: Side) -> Result<Merged, Error> {
    type Key<'a> = (Class, &'a str);
    fn index(files: &[FileEntry]) -> BTreeMap<Key<'_>, &FileEntry> {
        files
            .iter()
            .map(|f| ((f.class, f.path.as_str()), f))
            .collect()
    }
    /// Mesmo estado para a decisão: os dois ausentes, ou o mesmo texto claro.
    fn same(x: Option<&FileEntry>, y: Option<&FileEntry>) -> bool {
        match (x, y) {
            (None, None) => true,
            (Some(x), Some(y)) => x.sha256 == y.sha256,
            _ => false,
        }
    }

    let (base_ix, left_ix, right_ix) = (
        index(base),
        index(&left.manifest.files),
        index(&right.manifest.files),
    );
    let keys: BTreeSet<Key> = base_ix
        .keys()
        .chain(left_ix.keys())
        .chain(right_ix.keys())
        .copied()
        .collect();
    // quem leva o lugar num conflito: created_at maior, depois id maior (simétrico)
    let left_wins =
        (left.manifest.created_at, left.id.hash) > (right.manifest.created_at, right.id.hash);

    let mut files: Vec<FileEntry> = Vec::new();
    let mut conflicts: Vec<Conflict> = Vec::new();
    for key in keys {
        let (b, l, r) = (
            base_ix.get(&key).copied(),
            left_ix.get(&key).copied(),
            right_ix.get(&key).copied(),
        );
        if same(l, r) {
            // mesmo texto claro: com blobs diferentes (nonce), fica o de menor hash
            if let (Some(l), Some(r)) = (l, r) {
                files.push(if r.blob.0 < l.blob.0 { r } else { l }.clone());
            }
        } else if same(l, b) {
            files.extend(r.cloned());
        } else if same(r, b) {
            files.extend(l.cloned());
        } else {
            match (l, r) {
                (Some(l), Some(r)) => {
                    let (winner, loser, loser_side) = if left_wins {
                        (l, r, right)
                    } else {
                        (r, l, left)
                    };
                    files.push(winner.clone());
                    conflicts.push(Conflict {
                        class: key.0,
                        path: key.1.to_string(),
                        kind: ConflictKind::BothEdited,
                        loser: Some(loser.clone()),
                        loser_device: loser_side.manifest.device.clone(),
                        copy_path: None,
                    });
                }
                // removido de um lado, editado do outro: a edição fica
                (Some(kept), None) | (None, Some(kept)) => {
                    let remover = if l.is_none() { left } else { right };
                    files.push(kept.clone());
                    conflicts.push(Conflict {
                        class: key.0,
                        path: key.1.to_string(),
                        kind: ConflictKind::DeletedVsEdited,
                        loser: None,
                        loser_device: remover.manifest.device.clone(),
                        copy_path: None,
                    });
                }
                // os dois ausentes já caiu em `same(l, r)`
                (None, None) => {}
            }
        }
    }

    // cópias `.conflito-*` da classe vault, em ordem fixa (os conflitos já vêm ordenados)
    let mut taken: HashSet<(Class, String)> = files
        .iter()
        .map(|f| (f.class, f.path.to_lowercase()))
        .collect();
    let mut copies = Vec::new();
    for conflict in &mut conflicts {
        let (Class::Vault, ConflictKind::BothEdited, Some(loser)) =
            (conflict.class, &conflict.kind, &conflict.loser)
        else {
            continue;
        };
        let loser_at = if left_wins {
            right.manifest.created_at
        } else {
            left.manifest.created_at
        };
        let stem = format!(
            "{}.conflito-{}-{}",
            conflict.path,
            conflict.loser_device,
            utc_stamp(loser_at)
        );
        let mut candidate = format!("{stem}.md");
        let mut n = 2;
        while taken.contains(&(Class::Vault, candidate.to_lowercase())) {
            candidate = format!("{stem}-{n}.md");
            n += 1;
        }
        validate_path(&candidate)?;
        taken.insert((Class::Vault, candidate.to_lowercase()));
        copies.push(FileEntry {
            path: candidate.clone(),
            class: Class::Vault,
            ..loser.clone()
        });
        conflict.copy_path = Some(candidate);
    }
    files.extend(copies);
    files.sort_by(|a, b| (a.class, &a.path).cmp(&(b.class, &b.path)));
    Ok(Merged { files, conflicts })
}

/// `AAAAMMDDTHHMMSSZ` em UTC a partir de segundos Unix (calendário gregoriano proléptico,
/// algoritmo `civil_from_days` de Howard Hinnant).
fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Hash32;
    use crate::FORMAT_VERSION;
    use std::collections::HashSet;

    // ---------- ajudantes (fusão) ----------

    fn h(b: u8) -> Hash32 {
        Hash32([b; 32])
    }

    /// Entrada cujo conteúdo é identificado por `content` (sha, blob e tamanho derivam dele).
    fn e(class: Class, path: &str, content: u8) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            class,
            blob: h(content.wrapping_add(100)),
            size: u64::from(content),
            sha256: h(content),
        }
    }

    fn v(path: &str, content: u8) -> FileEntry {
        e(Class::Vault, path, content)
    }

    fn mid(name: &str) -> ObjectId {
        ObjectId::of(crate::object::ObjectKind::Manifest, name.as_bytes())
    }

    fn manifest(device: &str, created_at: u64, mut files: Vec<FileEntry>) -> Manifest {
        files.sort_by(|a, b| (a.class, &a.path).cmp(&(b.class, &b.path)));
        Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: 1,
            base_version: None,
            merged_from: vec![],
            counter: 1,
            device: device.to_string(),
            created_at,
            files,
        }
    }

    type Spec<'a> = (&'a str, u64, Vec<FileEntry>);

    /// Funde `l` e `r` sobre `base`, e confere que a ordem dos lados não muda o resultado.
    fn run(base: &[FileEntry], l: Spec, r: Spec) -> Merged {
        try_run(base, l, r).unwrap()
    }

    fn try_run(base: &[FileEntry], l: Spec, r: Spec) -> Result<Merged, Error> {
        let (ml, mr) = (manifest(l.0, l.1, l.2), manifest(r.0, r.1, r.2));
        let left = Side {
            id: mid("left"),
            manifest: &ml,
        };
        let right = Side {
            id: mid("right"),
            manifest: &mr,
        };
        let forward = merge_files(base, left, right);
        assert_eq!(forward, merge_files(base, right, left), "simetria");
        forward
    }

    fn paths(m: &Merged) -> Vec<(Class, &str, u8)> {
        m.files
            .iter()
            .map(|f| (f.class, f.path.as_str(), f.sha256.0[0]))
            .collect()
    }

    // ---------- fusão: a tabela da spec §7 ----------

    #[test]
    fn unchanged_files_are_kept() {
        let base = [v("a.md", 1)];
        let m = run(&base, ("pa", 10, base.to_vec()), ("pb", 20, base.to_vec()));
        assert_eq!(paths(&m), [(Class::Vault, "a.md", 1)]);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn a_change_on_only_one_side_is_applied() {
        let base = [v("a.md", 1), v("b.md", 1)];
        let m = run(
            &base,
            ("pa", 10, vec![v("a.md", 2), v("b.md", 1)]),
            ("pb", 20, vec![v("a.md", 1), v("b.md", 3)]),
        );
        assert_eq!(
            paths(&m),
            [(Class::Vault, "a.md", 2), (Class::Vault, "b.md", 3)]
        );
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn a_file_created_on_one_side_only_is_applied() {
        let m = run(&[], ("pa", 10, vec![v("novo.md", 5)]), ("pb", 20, vec![]));
        assert_eq!(paths(&m), [(Class::Vault, "novo.md", 5)]);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn a_removal_on_one_side_is_applied_when_the_other_did_not_touch_it() {
        let base = [v("a.md", 1), v("b.md", 1)];
        let m = run(
            &base,
            ("pa", 10, vec![v("b.md", 1)]),
            ("pb", 20, base.to_vec()),
        );
        assert_eq!(paths(&m), [(Class::Vault, "b.md", 1)]);
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn removed_on_both_sides_stays_removed() {
        let m = run(&[v("a.md", 1)], ("pa", 10, vec![]), ("pb", 20, vec![]));
        assert!(m.files.is_empty() && m.conflicts.is_empty());
    }

    #[test]
    fn the_same_change_on_both_sides_is_not_a_conflict() {
        let m = run(
            &[v("a.md", 1)],
            ("pa", 10, vec![v("a.md", 2)]),
            ("pb", 20, vec![v("a.md", 2)]),
        );
        assert_eq!(paths(&m), [(Class::Vault, "a.md", 2)]);
        assert!(m.conflicts.is_empty());
    }

    /// Mesmo texto claro, blobs diferentes (nonce aleatório): fica o de menor hash de blob,
    /// qualquer que seja o lado.
    #[test]
    fn the_same_content_with_different_blobs_keeps_the_smaller_blob() {
        let mut a = v("a.md", 2);
        let mut b = v("a.md", 2);
        a.blob = h(9);
        b.blob = h(4);
        let m = run(&[v("a.md", 1)], ("pa", 10, vec![a]), ("pb", 20, vec![b]));
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].blob, h(4));
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn both_created_the_same_path_with_different_content_is_a_conflict() {
        let m = run(
            &[],
            ("pa", 10, vec![v("a.md", 1)]),
            ("pb", 20, vec![v("a.md", 2)]),
        );
        assert_eq!(m.conflicts.len(), 1);
        assert_eq!(m.conflicts[0].kind, ConflictKind::BothEdited);
    }

    // ---------- fusão: conflito na classe vault ----------

    #[test]
    fn both_edited_in_the_vault_keeps_the_newer_in_place_and_a_conflict_copy_of_the_other() {
        let m = run(
            &[v("a.md", 1)],
            ("pc-a", 200, vec![v("a.md", 2)]),
            ("pc-b", 100, vec![v("a.md", 3)]),
        );
        // o vencedor é o de created_at maior (200); o perdedor (100) vira cópia
        let copy = "a.md.conflito-pc-b-19700101T000140Z.md";
        assert_eq!(
            paths(&m),
            [(Class::Vault, "a.md", 2), (Class::Vault, copy, 3)]
        );
        // a cópia reaproveita o blob do perdedor (sem recifrar)
        assert_eq!(m.files[1].blob, v("x", 3).blob);
        assert_eq!(
            m.conflicts,
            [Conflict {
                class: Class::Vault,
                path: "a.md".to_string(),
                kind: ConflictKind::BothEdited,
                loser: Some(v("a.md", 3)),
                loser_device: "pc-b".to_string(),
                copy_path: Some(copy.to_string()),
            }]
        );
    }

    #[test]
    fn the_conflict_copy_date_is_the_losers_created_at_in_utc() {
        let m = run(
            &[v("a.md", 1)],
            ("pc-a", 1_793_455_331, vec![v("a.md", 2)]),
            ("pc-b", 1_793_455_332, vec![v("a.md", 3)]),
        );
        assert_eq!(
            m.conflicts[0].copy_path.as_deref(),
            Some("a.md.conflito-pc-a-20261031T140211Z.md")
        );
        // ano bissexto: 2000-02-29T00:00:00Z
        let m = run(
            &[v("a.md", 1)],
            ("pc-a", 951_782_400, vec![v("a.md", 2)]),
            ("pc-b", 951_782_401, vec![v("a.md", 3)]),
        );
        assert_eq!(
            m.conflicts[0].copy_path.as_deref(),
            Some("a.md.conflito-pc-a-20000229T000000Z.md")
        );
    }

    #[test]
    fn the_copy_goes_next_to_the_original_in_its_folder() {
        let m = run(
            &[v("notas/x.md", 1)],
            ("pa", 20, vec![v("notas/x.md", 2)]),
            ("pb", 10, vec![v("notas/x.md", 3)]),
        );
        assert_eq!(
            m.conflicts[0].copy_path.as_deref(),
            Some("notas/x.md.conflito-pb-19700101T000010Z.md")
        );
    }

    /// Mesmo `created_at`: ganha o manifest de id maior; o resultado é o mesmo nos dois sentidos.
    #[test]
    fn a_created_at_tie_is_broken_by_the_larger_manifest_id() {
        let m = run(
            &[v("a.md", 1)],
            ("pa", 50, vec![v("a.md", 2)]),
            ("pb", 50, vec![v("a.md", 3)]),
        );
        let left_wins = mid("left").hash > mid("right").hash;
        let (winner, loser_device) = if left_wins { (2, "pb") } else { (3, "pa") };
        assert_eq!(m.files[0].sha256, h(winner));
        assert_eq!(m.conflicts[0].loser_device, loser_device);
    }

    #[test]
    fn a_taken_copy_name_gets_a_numeric_suffix_case_insensitively() {
        let taken = "a.md.conflito-pb-19700101T000010Z.md";
        let upper = taken.to_uppercase();
        let base = [v("a.md", 1), v(taken, 7), v(&upper, 8)];
        let m = run(
            &base,
            ("pa", 20, vec![v("a.md", 2), v(taken, 7), v(&upper, 8)]),
            ("pb", 10, vec![v("a.md", 3), v(taken, 7), v(&upper, 8)]),
        );
        assert_eq!(
            m.conflicts[0].copy_path.as_deref(),
            Some("a.md.conflito-pb-19700101T000010Z-2.md")
        );
        assert_eq!(m.files.len(), 4);
    }

    #[test]
    fn two_conflicts_get_independent_copies() {
        let base = [v("a.md", 1), v("b.md", 1)];
        let m = run(
            &base,
            ("pa", 20, vec![v("a.md", 2), v("b.md", 2)]),
            ("pb", 10, vec![v("a.md", 3), v("b.md", 3)]),
        );
        assert_eq!(m.conflicts.len(), 2);
        assert_eq!(m.files.len(), 4);
    }

    #[test]
    fn a_copy_name_too_long_for_a_path_component_is_an_error_not_a_lost_file() {
        let long = "n".repeat(250);
        let name = format!("{long}.md");
        let result = try_run(
            &[v(&name, 1)],
            ("pa", 20, vec![v(&name, 2)]),
            ("pb", 10, vec![v(&name, 3)]),
        );
        assert!(matches!(result, Err(Error::InvalidPath(_))), "{result:?}");
    }

    // ---------- fusão: classes config e secret (ADR 0010) ----------

    #[test]
    fn config_and_secret_conflicts_keep_the_winner_and_make_no_copy_entry() {
        for class in [Class::Config, Class::Secret] {
            let m = run(
                &[e(class, "x.json", 1)],
                ("pa", 20, vec![e(class, "x.json", 2)]),
                ("pb", 10, vec![e(class, "x.json", 3)]),
            );
            assert_eq!(paths(&m), [(class, "x.json", 2)], "{class:?}");
            assert_eq!(
                m.conflicts,
                [Conflict {
                    class,
                    path: "x.json".to_string(),
                    kind: ConflictKind::BothEdited,
                    loser: Some(e(class, "x.json", 3)),
                    loser_device: "pb".to_string(),
                    copy_path: None,
                }]
            );
        }
    }

    // ---------- fusão: removido de um lado, editado do outro ----------

    #[test]
    fn deleted_on_one_side_and_edited_on_the_other_keeps_the_edit_and_records_it() {
        let m = run(
            &[v("a.md", 1)],
            ("pa", 10, vec![]),
            ("pb", 20, vec![v("a.md", 2)]),
        );
        assert_eq!(paths(&m), [(Class::Vault, "a.md", 2)]);
        assert_eq!(
            m.conflicts,
            [Conflict {
                class: Class::Vault,
                path: "a.md".to_string(),
                kind: ConflictKind::DeletedVsEdited,
                loser: None,
                loser_device: "pa".to_string(),
                copy_path: None,
            }]
        );
    }

    #[test]
    fn the_edit_wins_over_the_deletion_even_when_the_deletion_is_newer() {
        let m = run(
            &[v("a.md", 1)],
            ("pa", 999, vec![]),
            ("pb", 1, vec![v("a.md", 2)]),
        );
        assert_eq!(paths(&m), [(Class::Vault, "a.md", 2)]);
        assert_eq!(m.conflicts[0].kind, ConflictKind::DeletedVsEdited);
    }

    // ---------- fusão: classes e ordem ----------

    #[test]
    fn the_same_path_in_different_classes_is_independent() {
        let base = [v("config.toml", 1), e(Class::Config, "config.toml", 1)];
        let m = run(
            &base,
            (
                "pa",
                10,
                vec![v("config.toml", 2), e(Class::Config, "config.toml", 1)],
            ),
            (
                "pb",
                20,
                vec![v("config.toml", 1), e(Class::Config, "config.toml", 3)],
            ),
        );
        assert_eq!(
            paths(&m),
            [
                (Class::Vault, "config.toml", 2),
                (Class::Config, "config.toml", 3)
            ]
        );
        assert!(m.conflicts.is_empty());
    }

    #[test]
    fn an_empty_base_makes_every_difference_a_conflict() {
        // histórico parcial: sem ancestral, nada pode ser dado como "só um lado mudou"
        let m = run(
            &[],
            ("pa", 10, vec![v("a.md", 1), v("b.md", 5)]),
            ("pb", 20, vec![v("a.md", 2), v("b.md", 5)]),
        );
        assert_eq!(m.conflicts.len(), 1);
        assert_eq!(m.conflicts[0].path, "a.md");
        assert_eq!(m.files.len(), 3); // a.md, a.md.conflito-*, b.md
    }

    // ---------- fusão: propriedades ----------

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn random_files(rng: &mut Rng) -> Vec<FileEntry> {
        let mut out = Vec::new();
        for class in [Class::Vault, Class::Config, Class::Secret] {
            for path in ["a.md", "b.md", "d/c.md"] {
                if !rng.next().is_multiple_of(3) {
                    out.push(e(class, path, 1 + (rng.next() % 3) as u8));
                }
            }
        }
        out
    }

    fn key(f: &FileEntry) -> (Class, &str) {
        (f.class, f.path.as_str())
    }

    /// Sobre bases e lados aleatórios: simétrica (já conferido por `run`), válida como
    /// manifest, idempotente e, acima de tudo, **nenhum conteúdo some em silêncio**.
    #[test]
    fn random_merges_are_valid_and_never_lose_content() {
        let mut rng = Rng(0x00C0_FFEE_1234_5678);
        for round in 0..500 {
            let base = random_files(&mut rng);
            let left = random_files(&mut rng);
            let right = random_files(&mut rng);
            let ts = |rng: &mut Rng| [10u64, 20, 20][(rng.next() % 3) as usize];
            let (lt, rt) = (ts(&mut rng), ts(&mut rng));
            let m = run(&base, ("pa", lt, left.clone()), ("pb", rt, right.clone()));

            // 1. o resultado cabe num manifest válido (ordem, unicidade, nomes)
            let candidate = manifest("pa", 1, m.files.clone());
            assert_eq!(candidate.validate(), Ok(()), "round {round}");
            assert_eq!(candidate.files, m.files, "round {round}: ordem");

            // 2. nenhum conteúdo some: todo sha de um lado está no resultado, é um perdedor
            //    registrado, ou é igual à base (esse lado não mexeu, e o outro lado decidiu
            //    mudar ou remover)
            let kept: HashSet<Hash32> = m
                .files
                .iter()
                .map(|f| f.sha256)
                .chain(
                    m.conflicts
                        .iter()
                        .filter_map(|c| c.loser.as_ref().map(|f| f.sha256)),
                )
                .collect();
            for mine in [&left, &right] {
                for f in mine {
                    if kept.contains(&f.sha256) {
                        continue;
                    }
                    let in_base = base
                        .iter()
                        .any(|b| key(b) == key(f) && b.sha256 == f.sha256);
                    assert!(in_base, "round {round}: perdeu {f:?}");
                }
            }

            // 3. cada cópia citada num conflito existe nos files com o blob do perdedor
            for c in &m.conflicts {
                if let Some(copy) = &c.copy_path {
                    let entry = m.files.iter().find(|f| f.path == *copy).unwrap();
                    assert_eq!(Some(entry.blob), c.loser.as_ref().map(|l| l.blob));
                }
            }

            // 4. idempotente: dois lados iguais não geram conflito e dão o próprio estado
            let same = run(&base, ("pa", lt, left.clone()), ("pb", rt, left.clone()));
            assert!(same.conflicts.is_empty(), "round {round}");
            assert_eq!(same.files, manifest("pa", 1, left.clone()).files);
        }
    }

    // ---------- ancestral comum ----------

    fn g(name: &str, counter: u64, base: Option<&str>, merged: &[&str]) -> (ObjectId, Manifest) {
        let mut m = manifest("d", 0, vec![]);
        m.counter = counter;
        m.base_version = base.map(|b| Hash32(mid(b).hash));
        m.merged_from = merged.iter().map(|x| Hash32(mid(x).hash)).collect();
        (mid(name), m)
    }

    fn anc(all: &[(ObjectId, Manifest)], a: &str, b: &str) -> Option<ObjectId> {
        let forward = common_ancestor(all, &mid(a), &mid(b));
        assert_eq!(forward, common_ancestor(all, &mid(b), &mid(a)), "simetria");
        forward
    }

    #[test]
    fn two_children_of_the_same_base_meet_at_the_base() {
        let all = [
            g("a", 1, None, &[]),
            g("b", 2, Some("a"), &[]),
            g("c", 2, Some("a"), &[]),
        ];
        assert_eq!(anc(&all, "b", "c"), Some(mid("a")));
    }

    #[test]
    fn a_manifest_and_its_descendant_meet_at_the_manifest_itself() {
        let all = [
            g("a", 1, None, &[]),
            g("b", 2, Some("a"), &[]),
            g("c", 3, Some("b"), &[]),
        ];
        assert_eq!(anc(&all, "b", "c"), Some(mid("b")));
        assert_eq!(anc(&all, "c", "c"), Some(mid("c")));
    }

    #[test]
    fn unrelated_roots_or_a_missing_history_have_no_common_ancestor() {
        let all = [g("a", 1, None, &[]), g("b", 1, None, &[])];
        assert_eq!(anc(&all, "a", "b"), None);
        let partial = [
            g("x", 5, Some("nunca-visto"), &[]),
            g("y", 5, Some("nunca-visto"), &[]),
        ];
        assert_eq!(anc(&partial, "x", "y"), None);
    }

    #[test]
    fn merge_parents_count_as_ancestors() {
        // m funde b e c; d sai de b: o mais próximo de m e d é b
        let all = [
            g("a", 1, None, &[]),
            g("b", 2, Some("a"), &[]),
            g("c", 2, Some("a"), &[]),
            g("m", 3, Some("b"), &["c"]),
            g("d", 3, Some("b"), &[]),
        ];
        assert_eq!(anc(&all, "m", "d"), Some(mid("b")));
        // e c só é alcançado por `merged_from`
        assert_eq!(anc(&all, "m", "c"), Some(mid("c")));
    }

    /// Fusões cruzadas: m1 e m2 fundem b e c em sentidos opostos. b e c são ancestrais comuns
    /// maximais; o desempate é fixo, qualquer que seja a ordem dos argumentos.
    #[test]
    fn criss_cross_merges_pick_one_maximal_ancestor_deterministically() {
        let all = [
            g("a", 1, None, &[]),
            g("b", 2, Some("a"), &[]),
            g("c", 2, Some("a"), &[]),
            g("m1", 3, Some("b"), &["c"]),
            g("m2", 3, Some("c"), &["b"]),
            g("x", 4, Some("m1"), &[]),
            g("y", 4, Some("m2"), &[]),
        ];
        let expected = if mid("b").hash > mid("c").hash {
            mid("b")
        } else {
            mid("c")
        };
        assert_eq!(anc(&all, "x", "y"), Some(expected));
    }

    /// O `counter` é conveniência, não causalidade: um ancestral com contador maior que o do
    /// descendente não pode ser escolhido no lugar do mais próximo.
    #[test]
    fn counter_does_not_decide_which_ancestor_is_closest() {
        let all = [
            g("a", 9, None, &[]),
            g("b", 1, Some("a"), &[]),
            g("c", 2, Some("b"), &[]),
            g("d", 2, Some("b"), &[]),
        ];
        assert_eq!(anc(&all, "c", "d"), Some(mid("b")));
    }

    /// Grafos aleatórios: o resultado, quando existe, é ancestral dos dois e maximal; é `None`
    /// só quando não há ancestral comum; e não depende da ordem da lista nem dos argumentos.
    #[test]
    fn random_graphs_give_a_maximal_common_ancestor_in_any_order() {
        fn reach(all: &[(ObjectId, Manifest)], from: &ObjectId) -> HashSet<[u8; 32]> {
            let mut seen = HashSet::new();
            let mut stack = vec![from.hash];
            while let Some(hash) = stack.pop() {
                if !seen.insert(hash) {
                    continue;
                }
                if let Some((_, m)) = all.iter().find(|(id, _)| id.hash == hash) {
                    stack.extend(m.base_version.iter().chain(&m.merged_from).map(|p| p.0));
                }
            }
            seen
        }
        let mut rng = Rng(0xBADC_0DE5_1357_9BDF);
        for round in 0..200 {
            let n = 2 + (rng.next() % 10) as usize;
            let names: Vec<String> = (0..n).map(|i| format!("g{round}-{i}")).collect();
            let mut all = Vec::new();
            for i in 0..n {
                let base = (i > 0 && !rng.next().is_multiple_of(4))
                    .then(|| names[(rng.next() as usize) % i].as_str());
                let merged: Vec<&str> = if i > 1 && rng.next().is_multiple_of(3) {
                    vec![names[(rng.next() as usize) % i].as_str()]
                } else {
                    vec![]
                };
                // contadores embaralhados de propósito: não coincidem com a causalidade
                all.push(g(&names[i], rng.next() % 5, base, &merged));
            }
            for _ in 0..6 {
                let a = &names[(rng.next() as usize) % n];
                let b = &names[(rng.next() as usize) % n];
                let (ra, rb) = (reach(&all, &mid(a)), reach(&all, &mid(b)));
                let common: HashSet<_> = ra.intersection(&rb).copied().collect();
                let got = anc(&all, a, b);
                match got {
                    None => assert!(
                        !common
                            .iter()
                            .any(|h| all.iter().any(|(id, _)| id.hash == *h)),
                        "round {round}: havia ancestral comum"
                    ),
                    Some(id) => {
                        assert!(common.contains(&id.hash), "round {round}: não é comum");
                        // maximal: nenhum outro ancestral comum o tem como ancestral
                        for other in common.iter().filter(|h| **h != id.hash) {
                            let other_id = ObjectId {
                                kind: crate::object::ObjectKind::Manifest,
                                hash: *other,
                            };
                            assert!(
                                !reach(&all, &other_id).contains(&id.hash),
                                "round {round}: {id:?} não é maximal"
                            );
                        }
                    }
                }
                let mut shuffled = all.clone();
                for k in (1..shuffled.len()).rev() {
                    shuffled.swap(k, (rng.next() as usize) % (k + 1));
                }
                assert_eq!(anc(&shuffled, a, b), got, "round {round}: ordem da lista");
            }
        }
    }
}
