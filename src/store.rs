//! Icon-level view of a DLL's resources.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::icon::{IconGroup, IconImage, build_group_dir, load_image_file, parse_group_dir};
use crate::pe::{self, RT_GROUP_ICON, RT_ICON, Resource};
use crate::resid::ResId;
use crate::shortcut::{self, ShortcutReport};

const DEFAULT_LANG: u16 = 0x0409;

/// All resources of a DLL, with icons decoded into [`IconGroup`]s and every
/// other resource type carried through untouched.
#[derive(Clone, Default)]
pub struct IconStore {
    pub others: Vec<Resource>,
    pub groups: Vec<IconGroup>,
}

impl IconStore {
    /// Reads the icons of any PE file (read-only use: list / extract).
    pub fn load(path: &Path) -> Result<IconStore> {
        let image = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        IconStore::from_resources(pe::read_resources(&image)?)
    }

    /// Loads a DLL that we are about to rewrite; refuses anything but a
    /// resource-only DLL so that code-bearing files can't be clobbered.
    pub fn load_writable(path: &Path) -> Result<IconStore> {
        let image = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        pe::check_resource_only(&image).with_context(|| path.display().to_string())?;
        IconStore::from_resources(pe::read_resources(&image)?)
    }

    /// Writes the DLL via a temporary file so a failure never leaves it
    /// half-written, then repoints shortcuts whose icon index shifted.
    pub fn save(&self, path: &Path) -> Result<ShortcutReport> {
        let image = pe::build_dll(&self.to_resources()?)?;
        // Icon order of the file being replaced, to see how indices move.
        let old_ids: Vec<ResId> = IconStore::load(path)
            .map(|s| s.groups.into_iter().map(|g| g.id).collect())
            .unwrap_or_default();
        let Some(mut tmp_name) = path.file_name().map(|n| n.to_os_string()) else {
            bail!("{} is not a file path", path.display())
        };
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        fs::write(&tmp, &image).with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
        let remap: Vec<Option<usize>> = old_ids.iter().map(|id| self.find(id)).collect();
        let report = if remap.iter().enumerate().all(|(i, n)| *n == Some(i)) {
            ShortcutReport::default()
        } else {
            shortcut::fix_shortcuts(path, &remap)
        };
        self.refresh_shell_icons(path);
        Ok(report)
    }

    /// Explorer caches icons by `(path, index)` and does not notice that the
    /// DLL changed. After removing an icon the positions shift, so a shortcut
    /// pointing at `path,N` keeps showing the stale cached image of the old
    /// index N even though the Properties dialog (which reads the file fresh)
    /// shows the right one. Per-entry invalidation is unreliable (the cache key
    /// also includes flags we can't know, and the old last index is no longer
    /// in `self.groups`), so ask the shell to flush its whole icon cache.
    #[cfg(windows)]
    fn refresh_shell_icons(&self, _path: &Path) {
        use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        // SAFETY: SHCNE_ASSOCCHANGED takes no items; both pointers must be null.
        unsafe {
            SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null());
        }
    }

    #[cfg(not(windows))]
    fn refresh_shell_icons(&self, _path: &Path) {}

    /// Adds PNG/ICO files as icons: one icon per file, or a single multi-size
    /// icon when `group` is set. `id` may only be given when exactly one icon
    /// results; otherwise fresh integer IDs are assigned. Returns the new IDs.
    /// On error the store may be partially modified.
    pub fn add_files(
        &mut self,
        files: &[impl AsRef<Path>],
        group: bool,
        id: Option<ResId>,
        keep_png: bool,
    ) -> Result<Vec<ResId>> {
        ensure!(!files.is_empty(), "no files to add");
        let mut batches: Vec<Vec<IconImage>> = Vec::new();
        if group {
            let mut images = Vec::new();
            for f in files {
                images.extend(load_image_file(f.as_ref(), keep_png)?);
            }
            batches.push(images);
        } else {
            for f in files {
                batches.push(load_image_file(f.as_ref(), keep_png)?);
            }
        }
        ensure!(
            id.is_none() || batches.len() == 1,
            "an explicit ID can only be used when adding a single icon (combine the files into one icon)"
        );
        let mut added = Vec::new();
        for images in batches {
            let gid = match &id {
                Some(id) => id.clone(),
                None => ResId::Id(self.next_group_id()?),
            };
            self.add(IconGroup { id: gid.clone(), lang: 0, images })?;
            added.push(gid);
        }
        Ok(added)
    }

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
