//! Pontas do grafo de manifests (`SPEC-TECNICA.md` §6). Não há ponteiro mutável: cada
//! manifest aponta para o anterior por `base_version` (e o de fusão, também por
//! `merged_from`), e a "cabeça" do Vault é o conjunto dos manifests que ninguém cita.

use crate::manifest::Manifest;
use crate::object::ObjectId;
use std::collections::HashSet;

/// Um manifest já verificado e decifrado, com o id que ele tem no armazenamento.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub id: ObjectId,
    pub manifest: Manifest,
}

/// Pontas entre `manifests`: os que nenhum outro cita em `base_version` nem em
/// `merged_from`. Uma base ausente da lista (histórico parcial) é permitida. A saída é
/// determinística, ordenada por `(counter, hash do id)`, qualquer que seja a ordem de entrada.
///
/// **Fusões equivalentes contam como uma só** (Fase 2): dois dispositivos que fundem as mesmas
/// pontas ao mesmo tempo geram manifests com o mesmo `base_version`, `merged_from` e `files`, mas
/// ids diferentes (nonce aleatório). Eles formam um grupo: o grupo é uma ponta (a de menor hash)
/// enquanto nenhum membro for citado; se alguém construir sobre **qualquer** membro, o grupo
/// inteiro fica coberto. Só vale para manifests com `merged_from`; dois snapshots comuns
/// iguais seguem como duas pontas (divergência real).
pub fn tips(mut manifests: Vec<(ObjectId, Manifest)>) -> Vec<Head> {
    let cited: HashSet<[u8; 32]> = manifests
        .iter()
        .flat_map(|(_, m)| m.base_version.iter().chain(&m.merged_from))
        .map(|hash| hash.0)
        .collect();
    // por hash crescente: o primeiro de cada grupo é o de menor hash
    manifests.sort_by_key(|(id, _)| id.hash);
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, (_, manifest)) in manifests.iter().enumerate() {
        let twin = groups
            .iter_mut()
            .find(|group| equivalent_merges(&manifests[group[0]].1, manifest));
        match twin {
            Some(group) => group.push(i),
            None => groups.push(vec![i]),
        }
    }
    let mut heads: Vec<Head> = groups
        .into_iter()
        .filter(|group| !group.iter().any(|i| cited.contains(&manifests[*i].0.hash)))
        .map(|group| {
            let (id, manifest) = manifests[group[0]].clone();
            Head { id, manifest }
        })
        .collect();
    heads.sort_by_key(|h| (h.manifest.counter, h.id.hash));
    heads
}

/// Mesmos pais (`base_version` e o conjunto de `merged_from`), mesma época e mesmos `files`,
/// ambos manifests de fusão.
fn equivalent_merges(a: &Manifest, b: &Manifest) -> bool {
    fn parents(m: &Manifest) -> Vec<[u8; 32]> {
        let mut parents: Vec<[u8; 32]> = m.merged_from.iter().map(|h| h.0).collect();
        parents.sort_unstable();
        parents
    }
    !a.merged_from.is_empty()
        && !b.merged_from.is_empty()
        && a.key_epoch == b.key_epoch
        && a.base_version == b.base_version
        && parents(a) == parents(b)
        && a.files == b.files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Hash32;
    use crate::FORMAT_VERSION;

    // ---------- ajudantes ----------

    /// Id falso e estável: o `tips` só olha o hash do id.
    fn id(name: &str) -> ObjectId {
        ObjectId::of(crate::object::ObjectKind::Manifest, name.as_bytes())
    }

    fn mf(counter: u64, base: Option<&str>, merged: &[&str]) -> Manifest {
        Manifest {
            format_version: FORMAT_VERSION as u16,
            key_epoch: 1,
            base_version: base.map(|b| Hash32(id(b).hash)),
            merged_from: merged.iter().map(|m| Hash32(id(m).hash)).collect(),
            counter,
            device: "d".to_string(),
            created_at: 0,
            files: vec![],
        }
    }

    fn ids(heads: &[Head]) -> Vec<ObjectId> {
        heads.iter().map(|h| h.id).collect()
    }

    // ---------- casos ----------

    #[test]
    fn no_manifests_no_tips() {
        assert!(tips(vec![]).is_empty());
    }

    #[test]
    fn a_single_manifest_is_the_tip() {
        let heads = tips(vec![(id("a"), mf(1, None, &[]))]);
        assert_eq!(ids(&heads), [id("a")]);
        assert_eq!(heads[0].manifest, mf(1, None, &[]));
    }

    #[test]
    fn a_linear_chain_has_one_tip_the_last() {
        let heads = tips(vec![
            (id("c"), mf(3, Some("b"), &[])),
            (id("a"), mf(1, None, &[])),
            (id("b"), mf(2, Some("a"), &[])),
        ]);
        assert_eq!(ids(&heads), [id("c")]);
    }

    #[test]
    fn two_children_of_the_same_base_are_two_tips() {
        let heads = tips(vec![
            (id("a"), mf(1, None, &[])),
            (id("b"), mf(2, Some("a"), &[])),
            (id("c"), mf(2, Some("a"), &[])),
        ]);
        let mut expected = [id("b"), id("c")];
        expected.sort_by_key(|i| i.hash);
        assert_eq!(ids(&heads), expected);
    }

    #[test]
    fn tips_are_ordered_by_counter_then_hash() {
        let heads = tips(vec![
            (id("hi"), mf(9, Some("a"), &[])),
            (id("lo"), mf(2, Some("a"), &[])),
            (id("a"), mf(1, None, &[])),
        ]);
        assert_eq!(ids(&heads), [id("lo"), id("hi")]);
    }

    #[test]
    fn a_merge_manifest_brings_the_graph_back_to_one_tip() {
        let heads = tips(vec![
            (id("a"), mf(1, None, &[])),
            (id("b"), mf(2, Some("a"), &[])),
            (id("c"), mf(2, Some("a"), &[])),
            (id("m"), mf(3, Some("b"), &["c"])),
        ]);
        assert_eq!(ids(&heads), [id("m")]);
    }

    #[test]
    fn merged_from_alone_is_enough_to_cover_a_manifest() {
        // `m` não tem `c` como base, só em `merged_from`
        let heads = tips(vec![
            (id("c"), mf(2, None, &[])),
            (id("m"), mf(3, None, &["c"])),
        ]);
        assert_eq!(ids(&heads), [id("m")]);
    }

    #[test]
    fn a_missing_base_does_not_hide_the_manifest() {
        let heads = tips(vec![(id("x"), mf(5, Some("nunca-visto"), &[]))]);
        assert_eq!(ids(&heads), [id("x")]);
    }

    #[test]
    fn a_manifest_citing_a_missing_merge_parent_is_still_a_tip() {
        let heads = tips(vec![(id("m"), mf(5, None, &["nunca-visto"]))]);
        assert_eq!(ids(&heads), [id("m")]);
    }

    #[test]
    fn unrelated_roots_are_all_tips() {
        let heads = tips(vec![
            (id("a"), mf(1, None, &[])),
            (id("b"), mf(1, None, &[])),
            (id("c"), mf(1, None, &[])),
        ]);
        assert_eq!(heads.len(), 3);
    }

    // ---------- fusões equivalentes (P-24/Fase 2: ADR 0014) ----------

    /// Manifest de fusão: como `mf`, mas com `files` e dispositivo à escolha.
    fn merge_mf(base: &str, merged: &[&str], device: &str, files: &[(&str, u8)]) -> Manifest {
        let mut m = mf(5, Some(base), merged);
        m.device = device.to_string();
        m.files = files
            .iter()
            .map(|(path, byte)| crate::manifest::FileEntry {
                path: path.to_string(),
                class: crate::manifest::Class::Vault,
                blob: Hash32([*byte; 32]),
                size: 1,
                sha256: Hash32([*byte; 32]),
            })
            .collect();
        m
    }

    fn history() -> Vec<(ObjectId, Manifest)> {
        vec![
            (id("a"), mf(1, None, &[])),
            (id("b"), mf(2, Some("a"), &[])),
            (id("c"), mf(2, Some("a"), &[])),
        ]
    }

    fn smaller(x: &str, y: &str) -> ObjectId {
        if id(x).hash < id(y).hash {
            id(x)
        } else {
            id(y)
        }
    }

    /// Dois dispositivos fundiram as mesmas pontas ao mesmo tempo: mesmo `base_version`, mesmo
    /// `merged_from`, mesmos `files`, ids diferentes. É uma ponta só, a de menor hash.
    #[test]
    fn equivalent_merges_count_as_one_tip_the_smaller_hash() {
        let mut all = history();
        all.push((id("m1"), merge_mf("b", &["c"], "pc-a", &[("x.md", 1)])));
        all.push((id("m2"), merge_mf("b", &["c"], "pc-b", &[("x.md", 1)])));
        assert_eq!(ids(&tips(all)), [smaller("m1", "m2")]);
    }

    #[test]
    fn the_order_of_merged_from_does_not_matter_for_equivalence() {
        let mut all = history();
        all.push((id("d"), mf(2, Some("a"), &[])));
        all.push((id("m1"), merge_mf("b", &["c", "d"], "pc-a", &[])));
        all.push((id("m2"), merge_mf("b", &["d", "c"], "pc-b", &[])));
        assert_eq!(ids(&tips(all)), [smaller("m1", "m2")]);
    }

    #[test]
    fn merges_with_different_files_are_two_tips() {
        let mut all = history();
        all.push((id("m1"), merge_mf("b", &["c"], "pc-a", &[("x.md", 1)])));
        all.push((id("m2"), merge_mf("b", &["c"], "pc-b", &[("x.md", 2)])));
        assert_eq!(tips(all).len(), 2);
    }

    #[test]
    fn merges_with_different_parents_are_two_tips() {
        let mut all = history();
        all.push((id("d"), mf(2, Some("a"), &[])));
        all.push((id("m1"), merge_mf("b", &["c"], "pc-a", &[])));
        all.push((id("m2"), merge_mf("b", &["d"], "pc-b", &[])));
        assert_eq!(tips(all).len(), 2);
    }

    /// Dois snapshots comuns iguais, do mesmo pai, não são "fusões": seguem como duas pontas
    /// (a regra do colapso só vale para manifests com `merged_from`).
    #[test]
    fn identical_plain_snapshots_are_still_two_tips() {
        let mut all = history();
        all.push((id("s1"), merge_mf("b", &[], "pc-a", &[("x.md", 1)])));
        all.push((id("s2"), merge_mf("b", &[], "pc-b", &[("x.md", 1)])));
        assert_eq!(tips(all).len(), 3); // c, s1 e s2
    }

    /// Alguém construiu em cima de **um** dos gêmeos: o outro não pode voltar a aparecer como
    /// ponta, seja ele o de menor hash ou não.
    #[test]
    fn building_on_either_twin_covers_both() {
        for built_on in ["m1", "m2"] {
            let mut all = history();
            all.push((id("m1"), merge_mf("b", &["c"], "pc-a", &[("x.md", 1)])));
            all.push((id("m2"), merge_mf("b", &["c"], "pc-b", &[("x.md", 1)])));
            all.push((id("n"), mf(6, Some(built_on), &[])));
            assert_eq!(ids(&tips(all)), [id("n")], "sobre {built_on}");
        }
    }

    #[test]
    fn equivalent_merges_collapse_in_any_input_order() {
        let items = vec![
            (id("a"), mf(1, None, &[])),
            (id("b"), mf(2, Some("a"), &[])),
            (id("c"), mf(2, Some("a"), &[])),
            (id("m1"), merge_mf("b", &["c"], "pc-a", &[("x.md", 1)])),
            (id("m2"), merge_mf("b", &["c"], "pc-b", &[("x.md", 1)])),
            (id("m3"), merge_mf("b", &["c"], "pc-c", &[("x.md", 1)])),
        ];
        let expected = [[id("m1"), id("m2"), id("m3")]
            .into_iter()
            .min_by_key(|i| i.hash)
            .unwrap()];
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for _ in 0..50 {
            let mut shuffled = items.clone();
            for k in (1..shuffled.len()).rev() {
                shuffled.swap(k, (rng.next() as usize) % (k + 1));
            }
            assert_eq!(ids(&tips(shuffled)), expected);
        }
    }

    // ---------- propriedade ----------

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Grafo aleatório: cada manifest cita (ou não) um anterior como base e, às vezes, outro
    /// em `merged_from`. As pontas esperadas são calculadas por fora, de forma ingênua.
    #[test]
    fn tips_do_not_depend_on_input_order() {
        let mut rng = Rng(0xA5A5_1234_DEAD_BEEF);
        for round in 0..200 {
            let n = 1 + (rng.next() % 12) as usize;
            let names: Vec<String> = (0..n).map(|i| format!("m{round}-{i}")).collect();
            let mut items: Vec<(ObjectId, Manifest)> = Vec::new();
            for i in 0..n {
                let base = (i > 0 && !rng.next().is_multiple_of(4))
                    .then(|| names[(rng.next() as usize) % i].as_str());
                let merged: Vec<&str> = if i > 1 && rng.next().is_multiple_of(3) {
                    vec![names[(rng.next() as usize) % i].as_str()]
                } else {
                    vec![]
                };
                items.push((id(&names[i]), mf(i as u64, base, &merged)));
            }

            // esperado, de forma ingênua: o grupo de fusões equivalentes (mesma base e mesmo
            // `merged_from`; aqui todos têm `files` vazios) é uma ponta, a de menor hash, se
            // ninguém cita nenhum membro dele
            let cited_by_someone = |hash: [u8; 32]| {
                items.iter().any(|(_, m)| {
                    m.base_version == Some(Hash32(hash)) || m.merged_from.contains(&Hash32(hash))
                })
            };
            let mut expected: Vec<ObjectId> = items
                .iter()
                .filter(|(own, m)| {
                    let twins: Vec<[u8; 32]> = items
                        .iter()
                        .filter(|(other, o)| {
                            other == own
                                || (!m.merged_from.is_empty()
                                    && o.base_version == m.base_version
                                    && o.merged_from == m.merged_from)
                        })
                        .map(|(other, _)| other.hash)
                        .collect();
                    twins.iter().all(|t| *t >= own.hash)
                        && !twins.iter().any(|t| cited_by_someone(*t))
                })
                .map(|(own, _)| *own)
                .collect();
            expected.sort_by_key(|i| {
                let counter = items.iter().find(|(o, _)| o == i).unwrap().1.counter;
                (counter, i.hash)
            });

            let first = ids(&tips(items.clone()));
            assert_eq!(first, expected, "round {round}");
            for _ in 0..5 {
                // embaralha (Fisher-Yates)
                for k in (1..items.len()).rev() {
                    items.swap(k, (rng.next() as usize) % (k + 1));
                }
                assert_eq!(ids(&tips(items.clone())), first, "round {round}");
            }
        }
    }
}
