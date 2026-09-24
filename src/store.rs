//! Icon-level view of a DLL's resources.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};

use crate::icon::{IconGroup, build_group_dir, parse_group_dir};
use crate::pe::{RT_GROUP_ICON, RT_ICON, Resource};
use crate::resid::ResId;

const DEFAULT_LANG: u16 = 0x0409;

/// All resources of a DLL, with icons decoded into [`IconGroup`]s and every
/// other resource type carried through untouched.
#[derive(Default)]
pub struct IconStore {
    pub others: Vec<Resource>,
    pub groups: Vec<IconGroup>,
}

impl IconStore {
    pub fn from_resources(resources: Vec<Resource>) -> Result<IconStore> {
        let mut icons: BTreeMap<(u16, u16), Vec<u8>> = BTreeMap::new();
        let mut group_res = Vec::new();
        let mut others = Vec::new();
        for r in resources {
            match (&r.rtype, &r.name) {
                (ResId::Id(RT_ICON), ResId::Id(id)) => {
                    icons.insert((*id, r.lang), r.data);
                }
                (ResId::Id(RT_GROUP_ICON), _) => group_res.push(r),
                _ => others.push(r),
            }
        }
        let mut groups = Vec::new();
        for r in group_res {
            let lookup = |id: u16| {
                icons.get(&(id, r.lang)).cloned().or_else(|| {
                    // Fall back to any language with that ID.
                    icons.range((id, 0)..=(id, u16::MAX)).next().map(|(_, d)| d.clone())
                })
            };
            let images = parse_group_dir(&r.data, lookup)?;
            groups.push(IconGroup { id: r.name, lang: r.lang, images });
        }
        Ok(IconStore { others, groups })
    }

    /// Flattens back to resources, renumbering RT_ICON entries from 1.
    pub fn to_resources(&self) -> Result<Vec<Resource>> {
        let mut out = self.others.clone();
        let mut next: u16 = 1;
        for g in &self.groups {
            ensure!(
                (next as usize) + g.images.len() <= 0x1_0000,
                "too many icon images (RT_ICON IDs exhausted)"
            );
            out.push(Resource {
                rtype: ResId::Id(RT_GROUP_ICON),
                name: g.id.clone(),
                lang: g.lang,
                data: build_group_dir(&g.images, next),
            });
            for img in &g.images {
                out.push(Resource {
                    rtype: ResId::Id(RT_ICON),
                    name: ResId::Id(next),
                    lang: g.lang,
                    data: img.data.clone(),
                });
                next += 1;
            }
        }
        Ok(out)
    }

    /// Smallest unused integer group ID above every existing one.
    pub fn next_group_id(&self) -> Result<u16> {
        let max = self
            .groups
            .iter()
            .filter_map(|g| match g.id {
                ResId::Id(n) => Some(n),
                ResId::Name(_) => None,
            })
            .max();
        match max {
            None => Ok(1),
            Some(n) if n < u16::MAX => Ok(n + 1),
            Some(_) => anyhow::bail!("no free integer icon ID left; use --id with a name"),
        }
    }

    pub fn find(&self, id: &ResId) -> Option<usize> {
        self.groups.iter().position(|g| &g.id == id)
    }

    pub fn add(&mut self, mut group: IconGroup) -> Result<()> {
        ensure!(!group.images.is_empty(), "icon {} has no images", group.id);
        ensure!(self.find(&group.id).is_none(), "icon {} already exists", group.id);
        if group.lang == 0 {
            group.lang = DEFAULT_LANG;
        }
        group.images.sort_by_key(|i| i.sort_key());
        self.groups.push(group);
        self.groups.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(())
    }

    pub fn remove(&mut self, id: &ResId) -> Result<IconGroup> {
        let idx = self
            .find(id)
            .ok_or_else(|| anyhow::anyhow!("icon {id} not found"))?;
        Ok(self.groups.remove(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icon::{IconImage, rgba_to_dib};

    fn img(size: u32) -> IconImage {
        IconImage {
            width: size,
            height: size,
            color_count: 0,
            planes: 1,
            bit_count: 32,
            data: rgba_to_dib(size, size, &vec![128; (size * size * 4) as usize]),
        }
    }

    #[test]
    fn add_remove_round_trip() {
        let mut store = IconStore::default();
        store.others.push(Resource {
            rtype: ResId::Id(16),
            name: ResId::Id(1),
            lang: 0x409,
            data: b"version".to_vec(),
        });
        assert_eq!(store.next_group_id().unwrap(), 1);
        store
            .add(IconGroup { id: ResId::Id(1), lang: 0, images: vec![img(32), img(16)] })
            .unwrap();
        store
            .add(IconGroup { id: ResId::Name("APP".into()), lang: 0x411, images: vec![img(48)] })
            .unwrap();
        assert!(store.add(IconGroup { id: ResId::Id(1), lang: 0, images: vec![img(8)] }).is_err());
        assert_eq!(store.next_group_id().unwrap(), 2);

        let res = store.to_resources().unwrap();
        // 1 other + 2 groups + 3 icons
        assert_eq!(res.len(), 6);
        let back = IconStore::from_resources(res).unwrap();
        assert_eq!(back.others.len(), 1);
        assert_eq!(back.groups.len(), 2);
        // Names sort before IDs.
        assert_eq!(back.groups[0].id, ResId::Name("APP".into()));
        assert_eq!(back.groups[0].lang, 0x411);
        assert_eq!(back.groups[1].id, ResId::Id(1));
        assert_eq!(back.groups[1].lang, DEFAULT_LANG);
        assert_eq!(
            back.groups[1].images.iter().map(|i| i.width).collect::<Vec<_>>(),
            vec![16, 32]
        );

        let mut back = back;
        back.remove(&ResId::Id(1)).unwrap();
        assert!(back.remove(&ResId::Id(1)).is_err());
        assert_eq!(back.to_resources().unwrap().len(), 3);
    }
}
