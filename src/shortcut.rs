//! Keeps Windows shortcuts (.lnk) pointing at the right icon after the DLL's
//! icon order changes.
//!
//! A shortcut stores its icon as `(path, index)`, where a non-negative index
//! is the position of the RT_GROUP_ICON entry in the DLL. Removing an icon
//! (or adding one that sorts before others) shifts those positions, so every
//! shortcut using a later icon would suddenly show a different image. We scan
//! the usual shortcut folders and rewrite the index stored in the .lnk header.
//! Negative indices name a resource ID directly and never need fixing.

use std::path::Path;

/// Result of fixing shortcuts after a save.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutReport {
    /// Shortcuts whose icon index was rewritten.
    pub updated: usize,
    /// Shortcuts that used an icon that no longer exists (left unchanged).
    pub orphaned: usize,
    /// Shortcuts that needed a fix but could not be written.
    pub failed: usize,
}

impl ShortcutReport {
    /// A short Japanese summary for the GUI, or `None` when nothing happened.
    pub fn summary_ja(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.updated > 0 {
            parts.push(format!("ショートカット {} 件のアイコン番号を更新", self.updated));
        }
        if self.orphaned > 0 {
            parts.push(format!("削除したアイコンを使っていたショートカット {} 件", self.orphaned));
        }
        if self.failed > 0 {
            parts.push(format!("更新できなかったショートカット {} 件", self.failed));
        }
        (!parts.is_empty()).then(|| parts.join("、"))
    }

    pub fn summary_en(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.updated > 0 {
            parts.push(format!("updated icon index of {} shortcut(s)", self.updated));
        }
        if self.orphaned > 0 {
            parts.push(format!("{} shortcut(s) used a removed icon", self.orphaned));
        }
        if self.failed > 0 {
            parts.push(format!("{} shortcut(s) could not be updated", self.failed));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

const HEADER_SIZE: usize = 0x4C;
const ICON_INDEX_OFFSET: usize = 0x38;

const HAS_ID_LIST: u32 = 0x1;
const HAS_LINK_INFO: u32 = 0x2;
const HAS_NAME: u32 = 0x4;
const HAS_RELATIVE_PATH: u32 = 0x8;
const HAS_WORKING_DIR: u32 = 0x10;
const HAS_ARGUMENTS: u32 = 0x20;
const HAS_ICON_LOCATION: u32 = 0x40;
const IS_UNICODE: u32 = 0x80;
const HAS_EXP_ICON: u32 = 0x4000;

const ICON_ENV_SIGNATURE: u32 = 0xA000_0007;
const ICON_ENV_SIZE: usize = 0x314;

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn utf16_until_nul(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Extracts the (unexpanded) icon path and icon index of a .lnk file
/// (MS-SHLLINK). Returns `None` for non-shortcuts or shortcuts without a
/// custom icon.
pub fn icon_location(lnk: &[u8]) -> Option<(String, i32)> {
    if u32_at(lnk, 0)? as usize != HEADER_SIZE {
        return None;
    }
    let flags = u32_at(lnk, 0x14)?;
    let index = u32_at(lnk, ICON_INDEX_OFFSET)? as i32;
    let mut pos = HEADER_SIZE;
    if flags & HAS_ID_LIST != 0 {
        pos += 2 + u16_at(lnk, pos)? as usize;
    }
    if flags & HAS_LINK_INFO != 0 {
        pos += u32_at(lnk, pos)? as usize;
    }
    let char_size = if flags & IS_UNICODE != 0 { 2 } else { 1 };
    let mut icon_string = None;
    for flag in [HAS_NAME, HAS_RELATIVE_PATH, HAS_WORKING_DIR, HAS_ARGUMENTS, HAS_ICON_LOCATION] {
        if flags & flag == 0 {
            continue;
        }
        let len = u16_at(lnk, pos)? as usize * char_size;
        let data = lnk.get(pos + 2..pos + 2 + len)?;
        if flag == HAS_ICON_LOCATION {
            icon_string = Some(if char_size == 2 {
                utf16_until_nul(data)
            } else {
                String::from_utf8_lossy(data).into_owned()
            });
        }
        pos += 2 + len;
    }
    // The environment-variable form takes precedence when present.
    if flags & HAS_EXP_ICON != 0 {
        while let Some(size) = u32_at(lnk, pos).map(|s| s as usize) {
            if size < 8 {
                break;
            }
            if u32_at(lnk, pos + 4)? == ICON_ENV_SIGNATURE && size >= ICON_ENV_SIZE {
                let unicode = utf16_until_nul(lnk.get(pos + 8 + 260..pos + 8 + 260 + 520)?);
                if !unicode.is_empty() {
                    icon_string = Some(unicode);
                }
                break;
            }
            pos += size;
        }
    }
    icon_string.filter(|s| !s.is_empty()).map(|s| (s, index))
}

/// Overwrites the icon index in a .lnk header.
pub fn set_icon_index(lnk: &mut [u8], index: i32) {
    lnk[ICON_INDEX_OFFSET..ICON_INDEX_OFFSET + 4].copy_from_slice(&index.to_le_bytes());
}

/// Rewrites every shortcut that uses `dll`: icon index `i` becomes
/// `remap[i]`; `None` means that icon was removed.
#[cfg(windows)]
pub fn fix_shortcuts(dll: &Path, remap: &[Option<usize>]) -> ShortcutReport {
    let mut report = ShortcutReport::default();
    let Some(target) = normalize(dll) else { return report };
    let mut files = Vec::new();
    for dir in win::shortcut_dirs() {
        collect_lnk(&dir, 0, &mut files);
    }
    files.sort();
    files.dedup();
    for lnk_path in files {
        let Ok(mut data) = std::fs::read(&lnk_path) else { continue };
        let Some((icon, index)) = icon_location(&data) else { continue };
        if index < 0 {
            continue; // resource-ID reference: stable
        }
        let expanded = win::expand_env(&icon);
        if normalize(Path::new(&expanded)).as_deref() != Some(target.as_str()) {
            continue;
        }
        match remap.get(index as usize) {
            Some(Some(new)) if *new != index as usize => {
                set_icon_index(&mut data, *new as i32);
                if std::fs::write(&lnk_path, &data).is_ok() {
                    report.updated += 1;
                    win::notify_updated(&lnk_path);
                } else {
                    report.failed += 1;
                }
            }
            Some(None) => report.orphaned += 1,
            _ => {}
        }
    }
    report
}

#[cfg(not(windows))]
pub fn fix_shortcuts(_dll: &Path, _remap: &[Option<usize>]) -> ShortcutReport {
    ShortcutReport::default()
}

/// Case-insensitive absolute form of a path for comparison.
#[cfg(windows)]
fn normalize(p: &Path) -> Option<String> {
    let full = std::fs::canonicalize(p).ok()?;
    Some(full.to_string_lossy().to_lowercase())
}

#[cfg(windows)]
fn collect_lnk(dir: &Path, depth: u32, out: &mut Vec<std::path::PathBuf>) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let path = e.path();
        if ft.is_dir() {
            collect_lnk(&path, depth + 1, out);
        } else if ft.is_file()
            && path.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk"))
        {
            out.push(path);
        }
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Programs, FOLDERID_PublicDesktop,
        FOLDERID_QuickLaunch, SHCNE_UPDATEITEM, SHCNF_PATHW, SHChangeNotify, SHGetKnownFolderPath,
    };
    use windows_sys::core::{GUID, PWSTR};

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    fn known_folder(id: &GUID) -> Option<PathBuf> {
        let mut p: PWSTR = std::ptr::null_mut();
        // SAFETY: valid GUID pointer and out-pointer; the returned buffer is
        // freed with CoTaskMemFree as documented (also on failure).
        unsafe {
            let hr = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p);
            let result = if hr >= 0 && !p.is_null() {
                let len = (0..).take_while(|&i| *p.add(i) != 0).count();
                Some(PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(p, len))))
            } else {
                None
            };
            CoTaskMemFree(p as *const _);
            result
        }
    }

    /// Desktop (user + public), Start menu (user + all users), and Quick
    /// Launch, which also holds the taskbar's pinned shortcuts.
    pub fn shortcut_dirs() -> Vec<PathBuf> {
        [
            &FOLDERID_Desktop,
            &FOLDERID_PublicDesktop,
            &FOLDERID_Programs,
            &FOLDERID_CommonPrograms,
            &FOLDERID_QuickLaunch,
        ]
        .into_iter()
        .filter_map(known_folder)
        .collect()
    }

    pub fn expand_env(s: &str) -> String {
        if !s.contains('%') {
            return s.to_string();
        }
        let src = wide(std::ffi::OsStr::new(s));
        let mut buf = vec![0u16; 1024];
        loop {
            // SAFETY: src is NUL-terminated; buf has buf.len() elements.
            let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) }
                as usize;
            if n == 0 {
                return s.to_string();
            }
            if n <= buf.len() {
                return String::from_utf16_lossy(&buf[..n - 1]);
            }
            buf.resize(n, 0);
        }
    }

    pub fn notify_updated(path: &Path) {
        let w = wide(path.as_os_str());
        // SAFETY: SHCNF_PATHW takes a NUL-terminated wide path as item1.
        unsafe {
            SHChangeNotify(SHCNE_UPDATEITEM as i32, SHCNF_PATHW, w.as_ptr() as *const _, std::ptr::null());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// Minimal unicode .lnk: header + ICON_LOCATION string (+ optional
    /// IconEnvironmentDataBlock).
    fn make_lnk(icon: &str, index: i32, env_icon: Option<&str>) -> Vec<u8> {
        let mut flags = HAS_ICON_LOCATION | IS_UNICODE;
        if env_icon.is_some() {
            flags |= HAS_EXP_ICON;
        }
        let mut b = vec![0u8; HEADER_SIZE];
        b[0..4].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        b[0x14..0x18].copy_from_slice(&flags.to_le_bytes());
        b[ICON_INDEX_OFFSET..ICON_INDEX_OFFSET + 4].copy_from_slice(&index.to_le_bytes());
        b.extend((icon.encode_utf16().count() as u16).to_le_bytes());
        b.extend(utf16(icon));
        if let Some(env) = env_icon {
            let mut block = vec![0u8; ICON_ENV_SIZE];
            block[0..4].copy_from_slice(&(ICON_ENV_SIZE as u32).to_le_bytes());
            block[4..8].copy_from_slice(&ICON_ENV_SIGNATURE.to_le_bytes());
            let u = utf16(env);
            block[8 + 260..8 + 260 + u.len()].copy_from_slice(&u);
            b.extend(block);
        }
        b.extend([0u8; 4]); // terminal block
        b
    }

    #[test]
    fn reads_and_patches_icon_index() {
        let mut lnk = make_lnk(r"C:\icons\my.dll", 3, None);
        assert_eq!(icon_location(&lnk), Some((r"C:\icons\my.dll".into(), 3)));
        set_icon_index(&mut lnk, 2);
        assert_eq!(icon_location(&lnk), Some((r"C:\icons\my.dll".into(), 2)));
    }

    #[test]
    fn env_block_takes_precedence() {
        let lnk = make_lnk(r"C:\Users\me\my.dll", 1, Some(r"%USERPROFILE%\my.dll"));
        assert_eq!(icon_location(&lnk), Some((r"%USERPROFILE%\my.dll".into(), 1)));
    }

    #[test]
    fn rejects_non_shortcuts() {
        assert_eq!(icon_location(b"not a shortcut"), None);
        assert_eq!(icon_location(&make_lnk("", 0, None)), None);
    }
}
