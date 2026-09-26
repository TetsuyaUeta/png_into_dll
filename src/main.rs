//! png_into_dll: store PNG images as Windows icon resources in a .dll file.

mod gui;
mod icon;
mod pe;
mod resid;
mod store;

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};

use icon::to_ico;
use resid::ResId;
use store::IconStore;

#[derive(Parser)]
#[command(
    name = "png_into_dll",
    version,
    about = "Store PNG images as Windows icon resources in a .dll file",
    after_help = "Run without a command to open the GUI."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the graphical interface (the default when no command is given)
    Gui {
        /// DLL to open at startup
        dll: Option<PathBuf>,
    },
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
    let result = match Cli::parse().cmd {
        None => gui::run(None),
        Some(Cmd::Gui { dll }) => gui::run(dll),
        Some(cmd) => run(cmd),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Gui { .. } => unreachable!("handled in main"),
        Cmd::Create { dll, force } => {
            ensure!(force || !dll.exists(), "{} already exists (use --force to overwrite)", dll.display());
            IconStore::default().save(&dll)?;
            println!("created {}", dll.display());
        }
        Cmd::Add { dll, files, group, id, keep_png } => {
            let mut store = if dll.exists() { IconStore::load_writable(&dll)? } else { IconStore::default() };
            let id = id.as_deref().map(ResId::parse).transpose()?;
            if id.is_some() && !group && files.len() > 1 {
                anyhow::bail!("--id can only be used when adding a single icon (use --group to combine files)");
            }
            for gid in store.add_files(&files, group, id, keep_png)? {
                let g = &store.groups[store.find(&gid).expect("just added")];
                println!("added icon {gid}: {}", g.describe());
            }
            store.save(&dll)?;
        }
        Cmd::List { dll } => {
            let store = IconStore::load(&dll)?;
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
            let mut store = IconStore::load_writable(&dll)?;
            for id in &ids {
                let g = store.remove(&ResId::parse(id)?)?;
                println!("removed icon {}: {}", g.id, g.describe());
            }
            store.save(&dll)?;
        }
        Cmd::Extract { dll, id, out } => {
            let store = IconStore::load(&dll)?;
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
