//! png_into_dll: store PNG images as Windows icon resources in a .dll file.

mod icon;
mod pe;
mod resid;
mod store;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};

use icon::{IconGroup, load_image_file, to_ico};
use resid::ResId;
use store::IconStore;

#[derive(Parser)]
#[command(name = "png_into_dll", version, about = "Store PNG images as Windows icon resources in a .dll file")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an empty icon DLL
    Create {
        dll: PathBuf,
        /// Overwrite the file if it already exists
        #[arg(long)]
        force: bool,
    },
    /// Add PNG/ICO files as icons (creates the DLL if it does not exist)
    Add {
        dll: PathBuf,
        /// PNG or ICO files. Each file becomes one icon unless --group is given.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Combine all files into a single multi-size icon
        #[arg(long)]
        group: bool,
        /// Resource ID (number) or name for the new icon; requires that exactly one icon is added
        #[arg(long, value_name = "ID|NAME")]
        id: Option<String>,
        /// Store PNG data as-is instead of converting sizes below 256px to 32-bit bitmaps
        #[arg(long)]
        keep_png: bool,
    },
    /// List icons stored in a DLL (works on any .dll/.exe)
    List { dll: PathBuf },
    /// Remove icons by ID or name
    Remove {
        dll: PathBuf,
        #[arg(required = true, value_name = "ID|NAME")]
        ids: Vec<String>,
    },
    /// Extract an icon as a .ico file
    Extract {
        dll: PathBuf,
        #[arg(value_name = "ID|NAME")]
        id: String,
        out: PathBuf,
    },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Create { dll, force } => {
            ensure!(force || !dll.exists(), "{} already exists (use --force to overwrite)", dll.display());
            save(&dll, &IconStore::default())?;
            println!("created {}", dll.display());
        }
        Cmd::Add { dll, files, group, id, keep_png } => {
            let mut store = if dll.exists() { load_writable(&dll)? } else { IconStore::default() };
            let mut batches: Vec<Vec<icon::IconImage>> = Vec::new();
            if group {
                let mut images = Vec::new();
                for f in &files {
                    images.extend(load_image_file(f, keep_png)?);
                }
                batches.push(images);
            } else {
                for f in &files {
                    batches.push(load_image_file(f, keep_png)?);
                }
            }
            let id = id.as_deref().map(ResId::parse).transpose()?;
            if id.is_some() {
                ensure!(
                    batches.len() == 1,
                    "--id can only be used when adding a single icon (use --group to combine files)"
                );
            }
            for images in batches {
                let gid = match &id {
                    Some(id) => id.clone(),
                    None => ResId::Id(store.next_group_id()?),
                };
                store.add(IconGroup { id: gid.clone(), lang: 0, images })?;
                let g = &store.groups[store.find(&gid).expect("just added")];
                println!("added icon {gid}: {}", g.describe());
            }
            save(&dll, &store)?;
        }
        Cmd::List { dll } => {
            let image = fs::read(&dll).with_context(|| format!("cannot read {}", dll.display()))?;
            let store = IconStore::from_resources(pe::read_resources(&image)?)?;
            if store.groups.is_empty() {
                println!("no icons in {}", dll.display());
            } else {
                println!("{:<6} {:<20} {:<7} images", "index", "id", "lang");
                for (i, g) in store.groups.iter().enumerate() {
                    println!("{:<6} {:<20} {:#06x} {}", i, g.id.to_string(), g.lang, g.describe());
                }
            }
            if !store.others.is_empty() {
                println!("({} non-icon resource(s) also present)", store.others.len());
            }
        }
        Cmd::Remove { dll, ids } => {
            let mut store = load_writable(&dll)?;
            for id in &ids {
                let g = store.remove(&ResId::parse(id)?)?;
                println!("removed icon {}: {}", g.id, g.describe());
            }
            save(&dll, &store)?;
        }
        Cmd::Extract { dll, id, out } => {
            let image = fs::read(&dll).with_context(|| format!("cannot read {}", dll.display()))?;
            let store = IconStore::from_resources(pe::read_resources(&image)?)?;
            let id = ResId::parse(&id)?;
            let idx = store.find(&id).ok_or_else(|| anyhow::anyhow!("icon {id} not found"))?;
            let g = &store.groups[idx];
            ensure!(!g.images.is_empty(), "icon {id} has no image data");
            fs::write(&out, to_ico(&g.images)).with_context(|| format!("cannot write {}", out.display()))?;
            println!("extracted icon {id} ({}) to {}", g.describe(), out.display());
        }
    }
    Ok(())
}

/// Loads a DLL that we are about to rewrite; refuses anything but a
/// resource-only DLL so that code-bearing files can't be clobbered.
fn load_writable(dll: &Path) -> Result<IconStore> {
    let image = fs::read(dll).with_context(|| format!("cannot read {}", dll.display()))?;
    pe::check_resource_only(&image).with_context(|| dll.display().to_string())?;
    IconStore::from_resources(pe::read_resources(&image)?)
}

/// Writes the DLL via a temporary file so a failure never leaves it half-written.
fn save(dll: &Path, store: &IconStore) -> Result<()> {
    let image = pe::build_dll(&store.to_resources()?)?;
    let file_name = dll.file_name().map(|n| n.to_os_string());
    let Some(mut tmp_name) = file_name else { bail!("{} is not a file path", dll.display()) };
    tmp_name.push(".tmp");
    let tmp = dll.with_file_name(tmp_name);
    fs::write(&tmp, &image).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, dll).with_context(|| format!("cannot replace {}", dll.display()))?;
    Ok(())
}
