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
pub fn tips(manifests: Vec<(ObjectId, Manifest)>) -> Vec<Head> {
    let cited: HashSet<[u8; 32]> = manifests
        .iter()
        .flat_map(|(_, m)| m.base_version.iter().chain(&m.merged_from))
        .map(|hash| hash.0)
        .collect();
    let mut heads: Vec<Head> = manifests
        .into_iter()
        .filter(|(id, _)| !cited.contains(&id.hash))
        .map(|(id, manifest)| Head { id, manifest })
        .collect();
    heads.sort_by_key(|h| (h.manifest.counter, h.id.hash));
    heads
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

            // esperado: ninguém cita o hash
            let mut expected: Vec<ObjectId> = items
                .iter()
                .filter(|(own, _)| {
                    !items.iter().any(|(_, m)| {
                        m.base_version == Some(Hash32(own.hash))
                            || m.merged_from.contains(&Hash32(own.hash))
                    })
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
