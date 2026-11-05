//! Graphical interface (egui): open/create an icon DLL, add PNG/ICO files by
//! dialog or drag & drop, preview, remove and extract icons.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use eframe::egui::{self, Color32, RichText, Sense, TextureHandle, TextureOptions, Vec2};

use crate::icon::{IconGroup, IconImage, to_ico};
use crate::resid::ResId;
use crate::shortcut::ShortcutReport;
use crate::store::IconStore;

const THUMB: f32 = 32.0;
const DETAIL_MAX: f32 = 128.0;

pub fn run(dll: Option<PathBuf>) -> Result<()> {
    detach_console();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("png_into_dll")
            .with_inner_size([860.0, 560.0])
            .with_min_inner_size([560.0, 360.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "png_into_dll",
        options,
        Box::new(move |cc| {
            install_cjk_font(&cc.egui_ctx);
            let mut app = App::default();
            if let Some(path) = dll {
                app.open(path);
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("cannot start GUI: {e}"))
}

/// When started by double-clicking, Windows gives the (console-subsystem)
/// executable its own console window; drop it so only the GUI remains.
/// A console shared with a parent shell is left alone.
#[cfg(windows)]
fn detach_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut pids = [0u32; 2];
    // SAFETY: the buffer is valid for the given length; FreeConsole has no preconditions.
    unsafe {
        if GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) == 1 {
            FreeConsole();
        }
    }
}

#[cfg(not(windows))]
fn detach_console() {}

/// egui's built-in fonts have no CJK glyphs; borrow a system font so
/// Japanese labels and file names render.
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| fs::read(p).ok()) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
    // First for proportional text so Latin and Japanese share one baseline;
    // only a fallback for monospace.
    fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "cjk".into());
    fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".into());
    ctx.set_fonts(fonts);
}

struct Status {
    text: String,
    error: bool,
}

#[derive(Default)]
struct App {
    path: Option<PathBuf>,
    store: IconStore,
    /// False for DLL/EXE files we must not rewrite (anything but resource-only).
    writable: bool,
    selected: Option<usize>,
    /// Decoded previews per group index, one entry per image.
    textures: HashMap<usize, Vec<Option<TextureHandle>>>,
    group_files: bool,
    keep_png: bool,
    id_text: String,
    confirm_remove: bool,
    status: Option<Status>,
}

/// Appends what happened to shortcuts (if anything) to a status message.
fn with_report(msg: String, report: &ShortcutReport) -> String {
    match report.summary_ja() {
        Some(s) => format!("{msg} ({s})"),
        None => msg,
    }
}

impl App {
    fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), error: false });
    }

    fn error(&mut self, context: &str, e: anyhow::Error) {
        self.status = Some(Status { text: format!("{context}: {e:#}"), error: true });
    }

    fn set_store(&mut self, store: IconStore, select: Option<&ResId>) {
        self.selected = select.and_then(|id| store.find(id));
        self.store = store;
        self.textures.clear();
    }

    fn open(&mut self, path: PathBuf) {
        match IconStore::load_writable(&path) {
            Ok(store) => {
                self.writable = true;
                self.set_store(store, None);
                self.info(format!("{} を開きました ({} 個のアイコン)", path.display(), self.store.groups.len()));
                self.path = Some(path);
            }
            // Not ours to rewrite, but still worth browsing and extracting from.
            Err(write_err) => match IconStore::load(&path) {
                Ok(store) => {
                    self.writable = false;
                    self.set_store(store, None);
                    self.status = Some(Status {
                        text: format!("読み取り専用で開きました (追加・削除不可): {write_err:#}"),
                        error: true,
                    });
                    self.path = Some(path);
                }
                Err(e) => self.error("開けませんでした", e),
            },
        }
    }

    fn reload(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let selected = self.selected_group().map(|g| g.id.clone());
        let result = if self.writable { IconStore::load_writable(&path) } else { IconStore::load(&path) };
        match result {
            Ok(store) => {
                self.set_store(store, selected.as_ref());
                self.info("再読み込みしました");
            }
            Err(e) => self.error("再読み込みに失敗しました", e),
        }
    }

    fn create_dialog(&mut self) -> bool {
        let Some(path) = rfd::FileDialog::new()
            .set_title("新しいアイコン DLL")
            .add_filter("DLL", &["dll"])
            .set_file_name("icons.dll")
            .save_file()
        else {
            return false;
        };
        match IconStore::default().save(&path) {
            Ok(_) => {
                self.writable = true;
                self.set_store(IconStore::default(), None);
                self.info(format!("{} を作成しました", path.display()));
                self.path = Some(path);
                true
            }
            Err(e) => {
                self.error("作成できませんでした", e);
                false
            }
        }
    }

    fn open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("アイコン DLL を開く")
            .add_filter("DLL / EXE", &["dll", "exe"])
            .add_filter("すべてのファイル", &["*"])
            .pick_file()
        {
            self.open(path);
        }
    }

    fn add_dialog(&mut self) {
        if let Some(files) = rfd::FileDialog::new()
            .set_title("追加する PNG / ICO")
            .add_filter("PNG / ICO", &["png", "ico"])
            .add_filter("すべてのファイル", &["*"])
            .pick_files()
        {
            self.add_files(&files);
        }
    }

    /// Adds files with the current options and saves immediately. The store
    /// is only replaced once the file has been written successfully.
    fn add_files(&mut self, files: &[PathBuf]) {
        if files.is_empty() {
            return;
        }
        if self.path.is_none() && !self.create_dialog() {
            return;
        }
        if !self.writable {
            self.status = Some(Status { text: "この DLL は読み取り専用です".into(), error: true });
            return;
        }
        let path = self.path.clone().expect("checked above");
        let id_text = self.id_text.trim();
        let result = (|| {
            let id = if id_text.is_empty() { None } else { Some(ResId::parse(id_text)?) };
            let mut store = self.store.clone();
            let added = store.add_files(files, self.group_files, id, self.keep_png)?;
            let report = store.save(&path)?;
            Ok::<_, anyhow::Error>((store, added, report))
        })();
        match result {
            Ok((store, added, report)) => {
                let ids = added.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                self.set_store(store, added.last());
                self.id_text.clear();
                self.info(with_report(format!("アイコン {ids} を追加しました"), &report));
            }
            Err(e) => self.error("追加できませんでした", e),
        }
    }

    fn remove_selected(&mut self) {
        let (Some(path), Some(idx)) = (self.path.clone(), self.selected) else { return };
        let mut store = self.store.clone();
        let group = store.groups.remove(idx);
        match store.save(&path) {
            Ok(report) => {
                let next = store.groups.get(idx.min(store.groups.len().saturating_sub(1))).map(|g| g.id.clone());
                self.set_store(store, next.as_ref());
                self.info(with_report(format!("アイコン {} を削除しました", group.id), &report));
            }
            Err(e) => self.error("削除できませんでした", e),
        }
    }

    fn extract_selected(&mut self) {
        let Some(group) = self.selected_group().cloned() else { return };
        let Some(out) = rfd::FileDialog::new()
            .set_title(".ico として保存")
            .add_filter("ICO", &["ico"])
            .set_file_name(format!("{}.ico", group.id))
            .save_file()
        else {
            return;
        };
        match fs::write(&out, to_ico(&group.images)) {
            Ok(()) => self.info(format!("{} に保存しました", out.display())),
            Err(e) => self.error("保存できませんでした", e.into()),
        }
    }

    fn selected_group(&self) -> Option<&IconGroup> {
        self.selected.and_then(|i| self.store.groups.get(i))
    }

    fn textures(&mut self, ctx: &egui::Context, idx: usize) -> &[Option<TextureHandle>] {
        let group = &self.store.groups[idx];
        self.textures.entry(idx).or_insert_with(|| {
            group
                .images
                .iter()
                .enumerate()
                .map(|(i, img)| {
                    let (w, h, rgba) = img.to_rgba().ok()?;
                    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    Some(ctx.load_texture(format!("icon-{idx}-{i}"), image, TextureOptions::LINEAR))
                })
                .collect()
        })
    }

    /// Windows `"path,index"` reference: `-ID` for integer IDs, otherwise the
    /// position in the resource directory (named icons come first).
    fn reference(&self, idx: usize) -> Option<String> {
        let path = self.path.as_ref()?;
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        let path = display_path(&path);
        Some(match &self.store.groups[idx].id {
            ResId::Id(n) => format!("{path},-{n}"),
            ResId::Name(_) => format!("{path},{idx}"),
        })
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("新規作成…").clicked() {
                self.create_dialog();
            }
            if ui.button("開く…").clicked() {
                self.open_dialog();
            }
            if ui.add_enabled(self.path.is_some(), egui::Button::new("再読み込み")).clicked() {
                self.reload();
            }
            ui.separator();
            match &self.path {
                Some(p) => {
                    if !self.writable {
                        ui.label(RichText::new("(読み取り専用)").color(ui.visuals().warn_fg_color));
                    }
                    let path = p.display().to_string();
                    ui.add(egui::Label::new(RichText::new(&path).strong()).truncate()).on_hover_text(path);
                }
                None => {
                    ui.weak("DLL が開かれていません");
                }
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            let can_add = self.path.is_none() || self.writable;
            if ui.add_enabled(can_add, egui::Button::new("ファイルを追加…")).clicked() {
                self.add_dialog();
            }
            ui.checkbox(&mut self.group_files, "複数ファイルを 1 つのアイコンにまとめる")
                .on_hover_text("16/32/48/256px などの PNG を 1 つのマルチサイズアイコンとして登録します");
            ui.checkbox(&mut self.keep_png, "PNG のまま格納")
                .on_hover_text("256px 未満も PNG のまま格納します (Vista 以降でのみ表示可能)");
            ui.label("ID / 名前:");
            ui.add(
                egui::TextEdit::singleline(&mut self.id_text)
                    .hint_text("自動")
                    .desired_width(90.0),
            )
            .on_hover_text("数字ならリソース ID、それ以外は名前。空欄なら最大 ID + 1");
        });
    }

    fn icon_list(&mut self, ui: &mut egui::Ui) {
        if self.store.groups.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak(if self.path.is_some() {
                    "アイコンがありません。PNG / ICO をここへドラッグ & ドロップするか「ファイルを追加…」を押してください。"
                } else {
                    "PNG / ICO をここへドラッグ & ドロップすると新しい DLL を作成して追加します。"
                });
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.add_space(THUMB + 8.0);
            header(ui, "index", 44.0);
            header(ui, "ID", 120.0);
            header(ui, "lang", 56.0);
            ui.strong("画像");
        });
        ui.separator();
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for idx in 0..self.store.groups.len() {
                let selected = self.selected == Some(idx);
                let thumb = best_image(&self.store.groups[idx].images, THUMB as u32)
                    .and_then(|i| self.textures(&ctx, idx)[i].clone());
                let group = &self.store.groups[idx];
                let bg = ui.painter().add(egui::Shape::Noop);
                let row = ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(THUMB), Sense::hover());
                    if let Some(tex) = &thumb {
                        egui::Image::new(tex).fit_to_exact_size(Vec2::splat(THUMB)).paint_at(ui, rect);
                    }
                    ui.add_space(4.0);
                    cell(ui, idx.to_string(), 44.0);
                    cell(ui, group.id.to_string(), 120.0);
                    cell(ui, format!("{:#06x}", group.lang), 56.0);
                    ui.add(egui::Label::new(group.describe()).truncate());
                });
                let rect = row.response.rect.expand2(Vec2::new(0.0, 2.0)).with_max_x(ui.max_rect().right());
                let resp = ui.interact(rect, ui.id().with(("row", idx)), Sense::click());
                let visuals = ui.visuals();
                let fill = if selected {
                    visuals.selection.bg_fill
                } else if resp.hovered() {
                    visuals.widgets.hovered.weak_bg_fill
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().set(bg, egui::Shape::rect_filled(rect, 4.0, fill));
                if resp.clicked() {
                    self.selected = Some(idx);
                }
                if resp.double_clicked() {
                    self.extract_selected();
                }
                ui.add_space(4.0);
            }
        });
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        let Some(idx) = self.selected.filter(|&i| i < self.store.groups.len()) else {
            ui.weak("アイコンを選択してください");
            return;
        };
        let ctx = ui.ctx().clone();
        let textures = self.textures(&ctx, idx).to_vec();
        let reference = self.reference(idx);
        let group = &self.store.groups[idx];
        let id = group.id.clone();

        ui.heading(format!("アイコン {id}"));
        egui::Grid::new("details").num_columns(2).show(ui, |ui| {
            ui.label("index");
            ui.label(idx.to_string());
            ui.end_row();
            ui.label("lang");
            ui.label(format!("{:#06x}", group.lang));
            ui.end_row();
        });
        if let Some(r) = &reference {
            ui.add_space(4.0);
            ui.label("参照 (ショートカット / レジストリ用):");
            ui.add(egui::Label::new(RichText::new(r).monospace()).wrap_mode(egui::TextWrapMode::Wrap));
            if ui.button("参照をコピー").clicked() {
                ctx.copy_text(r.clone());
                self.status = Some(Status { text: format!("コピーしました: {r}"), error: false });
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button(".ico として保存…").clicked() {
                self.extract_selected();
            }
            if ui.add_enabled(self.writable, egui::Button::new("削除")).clicked() {
                self.confirm_remove = true;
            }
        });
        ui.separator();

        let group = &self.store.groups[idx];
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for (img, tex) in group.images.iter().zip(&textures) {
                let size = Vec2::new(img.width as f32, img.height as f32);
                let shown = size * (DETAIL_MAX / size.max_elem()).min(1.0);
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(4.0)
                    .corner_radius(4.0)
                    .show(ui, |ui| match tex {
                        Some(tex) => {
                            ui.add(egui::Image::new(tex).fit_to_exact_size(shown));
                        }
                        None => {
                            ui.weak("(プレビュー不可)");
                        }
                    });
                ui.label(img.describe());
                ui.add_space(6.0);
            }
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_remove {
            return;
        }
        let Some(id) = self.selected_group().map(|g| g.id.clone()) else {
            self.confirm_remove = false;
            return;
        };
        let modal = egui::Modal::new(egui::Id::new("confirm_remove")).show(ctx, |ui| {
            ui.label(format!("アイコン {id} を削除して DLL を保存しますか？"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("削除").clicked() {
                    self.remove_selected();
                    ui.close();
                }
                if ui.button("キャンセル").clicked() {
                    ui.close();
                }
            });
        });
        if modal.should_close() {
            self.confirm_remove = false;
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let (hovering, dropped) = ctx.input(|i| {
            let dropped: Vec<PathBuf> = i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect();
            (!i.raw.hovered_files.is_empty(), dropped)
        });
        if hovering {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, "drop".into()));
            let rect = ctx.content_rect();
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(160));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "ドロップして追加",
                egui::FontId::proportional(28.0),
                Color32::WHITE,
            );
        }
        if dropped.is_empty() {
            return;
        }
        // A single dropped DLL/EXE opens it; anything else is added as icons.
        if let [single] = dropped.as_slice()
            && has_ext(single, &["dll", "exe"])
        {
            self.open(single.clone());
        } else {
            self.add_files(&dropped);
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) || self.store.groups.is_empty() {
            return;
        }
        let (up, down, delete) = ctx.input(|i| {
            (i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown), i.key_pressed(egui::Key::Delete))
        });
        let last = self.store.groups.len() - 1;
        if up {
            self.selected = Some(self.selected.map_or(0, |s| s.saturating_sub(1)));
        }
        if down {
            self.selected = Some(self.selected.map_or(0, |s| (s + 1).min(last)));
        }
        if delete && self.writable && self.selected.is_some() {
            self.confirm_remove = true;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let title = match &self.path {
            Some(p) => format!("{} - png_into_dll", p.file_name().unwrap_or_default().to_string_lossy()),
            None => "png_into_dll".to_string(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| match &self.status {
                Some(s) => {
                    let mut text = RichText::new(&s.text);
                    if s.error {
                        text = text.color(ui.visuals().error_fg_color);
                    }
                    ui.add(egui::Label::new(text).truncate()).on_hover_text(&s.text);
                }
                None => {
                    ui.weak("PNG / ICO ファイルをウィンドウへドラッグ & ドロップして追加できます");
                }
            });
        });
        egui::SidePanel::right("details").default_width(260.0).min_width(200.0).show(ctx, |ui| {
            ui.add_space(4.0);
            self.details(ui);
        });
        egui::CentralPanel::default().show(ctx, |ui| self.icon_list(ui));

        self.confirm_dialog(ctx);
        self.handle_keys(ctx);
        self.handle_drops(ctx);
    }
}

/// Picks the image to use as a thumbnail: the smallest one at least `target`
/// pixels wide, or the largest one if all are smaller.
fn best_image(images: &[IconImage], target: u32) -> Option<usize> {
    let by_size = |i: &usize| images[*i].width;
    (0..images.len())
        .filter(|&i| images[i].width >= target)
        .min_by_key(by_size)
        .or_else(|| (0..images.len()).max_by_key(by_size))
}

fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// Strips the `\\?\` prefix `canonicalize` adds on Windows.
fn display_path(path: &Path) -> String {
    let s = path.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

fn header(ui: &mut egui::Ui, text: &str, width: f32) {
    ui.add_sized([width, 16.0], egui::Label::new(RichText::new(text).strong()));
}

fn cell(ui: &mut egui::Ui, text: String, width: f32) {
    ui.add_sized([width, THUMB], egui::Label::new(text).truncate());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(size: u32) -> IconImage {
        IconImage { width: size, height: size, color_count: 0, planes: 1, bit_count: 32, data: Vec::new() }
    }

    #[test]
    fn thumbnail_choice() {
        assert_eq!(best_image(&[img(16), img(48), img(32), img(256)], 32), Some(2));
        assert_eq!(best_image(&[img(16), img(24)], 32), Some(1));
        assert_eq!(best_image(&[], 32), None);
    }

    #[test]
    fn strips_verbatim_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\x\icons.dll")), r"C:\x\icons.dll");
    }
}
