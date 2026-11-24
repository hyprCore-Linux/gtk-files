//! Formatting helpers, XDG user dirs, icons, and common file attributes.

use std::path::{Path, PathBuf};

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;

/// Attributes requested from GtkDirectoryList / FileInfo queries.
pub const FILE_ATTRIBUTES: &str = concat!(
    "standard::name,standard::display-name,standard::icon,",
    "standard::symbolic-icon,standard::content-type,standard::type,",
    "standard::size,standard::is-hidden,standard::is-backup,",
    "standard::is-symlink,standard::symlink-target,time::modified,access::can-read,",
    "access::can-write,access::can-execute,trash::orig-path,trash::deletion-date"
);

/// Remote listings (SFTP and other GVFS schemes). Omits content-type sniffing,
/// access checks, and symlink targets — each of those is another round trip.
pub const REMOTE_FILE_ATTRIBUTES: &str = concat!(
    "standard::name,standard::display-name,standard::icon,",
    "standard::symbolic-icon,standard::type,",
    "standard::size,standard::is-hidden,standard::is-backup,",
    "standard::is-symlink,time::modified,trash::orig-path,trash::deletion-date"
);

/// Attribute string for a directory listing. Remote mounts skip the expensive set.
pub fn listing_attributes(remote: bool) -> &'static str {
    if remote {
        REMOTE_FILE_ATTRIBUTES
    } else {
        FILE_ATTRIBUTES
    }
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub fn xdg_user_dir(key: &str, fallback: &str) -> PathBuf {
    let home = home_dir();
    let dirs_file = dirs::config_dir()
        .unwrap_or_else(|| home.join(".config"))
        .join("user-dirs.dirs");
    if let Ok(text) = std::fs::read_to_string(dirs_file) {
        let prefix = format!("XDG_{key}_DIR=");
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix(&prefix) {
                let val = rest.trim().trim_matches('"');
                let expanded = val.replace("$HOME", &home.to_string_lossy());
                let path = PathBuf::from(expanded);
                if path.exists() {
                    return path;
                }
            }
        }
    }
    home.join(fallback)
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

pub fn format_mtime(info: &gio::FileInfo) -> String {
    let Some(dt) = info.modification_date_time() else {
        return String::new();
    };
    dt.format("%Y-%m-%d %H:%M").unwrap_or_default().to_string()
}

pub fn display_name(info: &gio::FileInfo) -> String {
    info.display_name().to_string()
}

pub fn is_directory(info: &gio::FileInfo) -> bool {
    info.file_type() == gio::FileType::Directory
}

/// Hidden / backup flag — MTP and some GVFS backends omit these attributes
/// even when requested; calling `is_hidden()` then emits a GIO critical.
pub fn is_hidden(info: &gio::FileInfo) -> bool {
    let flagged = info.has_attribute(gio::FILE_ATTRIBUTE_STANDARD_IS_HIDDEN) && info.is_hidden();
    let backup = info.has_attribute(gio::FILE_ATTRIBUTE_STANDARD_IS_BACKUP) && info.is_backup();
    flagged || backup || display_name(info).starts_with('.')
}

/// Symlink flag — same MTP/GVFS caveat as [`is_hidden`].
pub fn is_symlink(info: &gio::FileInfo) -> bool {
    info.has_attribute(gio::FILE_ATTRIBUTE_STANDARD_IS_SYMLINK) && info.is_symlink()
}

/// Whether the current user can write/modify this item (from `access::can-write`).
/// Missing attribute → treat as writable so we don't flash false lock badges.
pub fn can_write(info: &gio::FileInfo) -> bool {
    if !info.has_attribute("access::can-write") {
        return true;
    }
    info.boolean("access::can-write")
}

/// Whether the on-disk entry itself is a symbolic link (not its target).
pub fn is_symlink_path(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Raw link target exactly as stored in the symlink (may be relative).
pub fn symlink_target(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path).ok()
}

/// Absolute link target: relative links are resolved against the link's parent.
pub fn resolved_symlink_target(path: &Path) -> Option<PathBuf> {
    let raw = symlink_target(path)?;
    if raw.is_absolute() {
        Some(raw)
    } else {
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        Some(parent.join(raw))
    }
}

#[allow(dead_code)]
pub fn content_type_label(info: &gio::FileInfo) -> String {
    if is_directory(info) {
        return "Folder".into();
    }
    effective_content_type(info).unwrap_or_else(|| "Unknown".into())
}

/// Content type already on the file info, or a local guess from the file name.
/// The guess does not read file bytes, so it is safe on SFTP.
pub fn effective_content_type(info: &gio::FileInfo) -> Option<String> {
    if let Some(ct) = info.content_type() {
        if !ct.is_empty() {
            return Some(ct.to_string());
        }
    }
    if is_directory(info) {
        return None;
    }
    let name = info.name();
    let (ctype, _uncertain) = gio::content_type_guess(Some(name.as_path()), &[]);
    if ctype.is_empty() {
        None
    } else {
        Some(ctype.to_string())
    }
}

pub fn icon_for_info(info: &gio::FileInfo, symbolic: bool) -> gio::Icon {
    let raw = if symbolic {
        info.symbolic_icon().or_else(|| info.icon())
    } else {
        info.icon()
    };
    // FileInfo often returns ThemedIcons (e.g. application-sql) that this
    // theme does not ship — GTK then paints image-missing (🚫). Prefer the
    // first name the theme actually has, else a plain text-file icon.
    let base = raw
        .and_then(|icon| usable_icon(icon))
        .or_else(|| icon_from_filename(info, symbolic))
        .unwrap_or_else(|| fallback_icon(info));

    // Prefer overlay badges in the list/grid views; still attach emblems here
    // for any caller that renders GIcons directly (e.g. fallbacks).
    let mut icon = base;
    if is_symlink(info) {
        let emblem_icon = gio::ThemedIcon::new("emblem-symbolic-link");
        let emblem = gio::Emblem::new(&emblem_icon);
        icon = gio::EmblemedIcon::new(&icon, Some(&emblem)).upcast();
    }
    if !can_write(info) {
        let emblem_icon = gio::ThemedIcon::new("changes-prevent-symbolic");
        let emblem = gio::Emblem::new(&emblem_icon);
        icon = gio::EmblemedIcon::new(&icon, Some(&emblem)).upcast();
    }
    icon
}

fn icon_theme() -> gtk::IconTheme {
    match gdk::Display::default() {
        Some(d) => gtk::IconTheme::for_display(&d),
        None => gtk::IconTheme::new(),
    }
}

/// Return `icon` if the theme can paint it; for ThemedIcons, pick the first
/// available name. Otherwise `None` so the caller can use a text-file fallback.
fn usable_icon(icon: gio::Icon) -> Option<gio::Icon> {
    let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() else {
        // FileIcon / BytesIcon / custom — keep as-is.
        return Some(icon);
    };
    let theme = icon_theme();
    for name in themed.names() {
        // Skip GTK's own missing-image sentinel names.
        if name == "image-missing" || name == "gtk-missing-image" {
            continue;
        }
        if theme.has_icon(&name) {
            return Some(gio::ThemedIcon::new(&name).upcast());
        }
    }
    None
}

/// Icon from the filename when the listing did not include `content-type`.
fn icon_from_filename(info: &gio::FileInfo, symbolic: bool) -> Option<gio::Icon> {
    if info.content_type().is_some() {
        return None;
    }
    let ct = effective_content_type(info)?;
    let icon = if symbolic {
        gio::content_type_get_symbolic_icon(&ct)
    } else {
        gio::content_type_get_icon(&ct)
    };
    usable_icon(icon)
}

fn fallback_icon(info: &gio::FileInfo) -> gio::Icon {
    let name = if is_directory(info) {
        "folder"
    } else {
        "text-x-generic"
    };
    gio::ThemedIcon::new(name).upcast()
}

pub fn file_from_dir_and_info(dir: &gio::File, info: &gio::FileInfo) -> gio::File {
    dir.child(info.name())
}

#[allow(dead_code)]
pub fn path_display(path: &Path) -> String {
    let home = home_dir();
    if let Ok(rel) = path.strip_prefix(&home) {
        if rel.as_os_str().is_empty() {
            return "Home".into();
        }
        return format!("~/{}", rel.display());
    }
    if path == Path::new("/") {
        return "Computer".into();
    }
    path.display().to_string()
}

pub fn title_for_location(file: &gio::File) -> String {
    if is_trash_location(file) {
        return "Trash".into();
    }
    if is_recent_location(file) {
        return "Recent".into();
    }
    if is_bookmarks_location(file) {
        return "Bookmarks".into();
    }
    if let Some(path) = file.path() {
        if path == home_dir() {
            return "Home".into();
        }
        if path.ends_with(".local/share/Trash/files") {
            return "Trash".into();
        }
        return path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
    }
    file.basename()
        .map(|b| b.to_string_lossy().to_string())
        .unwrap_or_else(|| file.uri().to_string())
}

/// Resolve a CLI / desktop-open argument to a folder (or trash) to browse.
///
/// Prefer [`gio::File::for_commandline_arg`] over `for_path` so URIs like
/// `trash:///` are not treated as relative filesystem paths.
pub fn location_from_open_arg(arg: impl AsRef<std::ffi::OsStr>, cwd: Option<&Path>) -> gio::File {
    let file = match cwd {
        Some(cwd) => gio::File::for_commandline_arg_and_cwd(arg, cwd),
        None => gio::File::for_commandline_arg(arg),
    };
    if is_trash_location(&file) {
        return trash_file();
    }
    if let Some(path) = file.path() {
        let s = path.to_string_lossy();
        // GVFS FUSE paths (mtp:, sftp:, …) — never call is_dir()/exists(); those
        // do synchronous remote I/O and can hang the UI on phone mounts.
        if s.contains("/gvfs/") {
            return gio::File::for_path(path);
        }
        if path.is_dir() {
            return gio::File::for_path(path);
        }
        if let Some(parent) = path.parent() {
            return gio::File::for_path(parent);
        }
    }
    // Remote / special URI (sftp://, trash://, …).
    file
}

pub fn is_trash_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s == "trash:///" || s.starts_with("trash:")
}

pub fn trash_file() -> gio::File {
    gio::File::for_uri("trash:///")
}

pub fn is_trash_location(file: &gio::File) -> bool {
    file.uri().starts_with("trash:")
}

/// Virtual location for the Recent folders panel (from places.toml).
pub fn is_recent_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s == "recent:///" || s.starts_with("recent:")
}

pub fn recent_file() -> gio::File {
    // Private scheme so DirectoryList / GVFS are never asked to enumerate it.
    gio::File::for_uri("recent:///")
}

pub fn is_recent_location(file: &gio::File) -> bool {
    file.uri().starts_with("recent:")
}

/// Virtual location for the Bookmarks panel (from places.toml).
pub fn is_bookmarks_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s == "bookmarks:///" || s.starts_with("bookmarks:")
}

pub fn bookmarks_file() -> gio::File {
    gio::File::for_uri("bookmarks:///")
}

pub fn is_bookmarks_location(file: &gio::File) -> bool {
    file.uri().starts_with("bookmarks:")
}

/// Recent or Bookmarks virtual panels (no real DirectoryList).
pub fn is_virtual_places_location(file: &gio::File) -> bool {
    is_recent_location(file) || is_bookmarks_location(file)
}

pub fn open_file_default(parent: Option<&impl IsA<gtk::Window>>, file: &gio::File) {
    match file.query_default_handler(None::<&gio::Cancellable>) {
        Ok(appinfo) => {
            if let Err(e) = appinfo.launch(&[file.clone()], None::<&gio::AppLaunchContext>) {
                eprintln!("launch failed: {e}");
                fallback_xdg_open(file);
            }
        }
        Err(e) => {
            eprintln!("no default handler: {e}");
            let launcher = gtk::FileLauncher::new(Some(file));
            let parent = parent.map(|w| w.clone().upcast::<gtk::Window>());
            launcher.launch(parent.as_ref(), None::<&gio::Cancellable>, move |res| {
                if let Err(err) = res {
                    eprintln!("FileLauncher failed: {err}");
                }
            });
            // Also try xdg-open immediately as a reliable fallback for local files.
            fallback_xdg_open(file);
        }
    }
}

fn fallback_xdg_open(file: &gio::File) {
    if let Some(path) = file.path() {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// Open a new gtk-term (or desktop terminal) with cwd `dir`.
pub fn open_terminal_here(dir: &Path) -> Result<(), String> {
    let dir = if dir.is_dir() {
        dir.to_path_buf()
    } else {
        dir.parent()
            .filter(|p| p.is_dir())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(home_dir)
    };
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()));
    }

    // Prefer the suite binary with an explicit cwd flag (works with a running
    // gtk-term primary instance via GApplication command-line).
    let suite_bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("gtk-term/target/release/gtk-term"));
    if let Some(bin) = suite_bin {
        if bin.is_file() {
            if std::process::Command::new(&bin)
                .arg("--working-directory")
                .arg(&dir)
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    if let Some(cfg) = dirs::config_dir() {
        let launch = cfg.join("gtk-apps/applications/gtk-term-launch.sh");
        if launch.is_file() {
            if std::process::Command::new(&launch)
                .arg("--working-directory")
                .arg(&dir)
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    for bin in [
        "gtk-term",
        "x-terminal-emulator",
        "kgx",
        "gnome-terminal",
        "xterm",
    ] {
        let mut cmd = std::process::Command::new(bin);
        match bin {
            "gtk-term" | "x-terminal-emulator" | "kgx" | "gnome-terminal" => {
                cmd.arg("--working-directory").arg(&dir);
            }
            _ => {
                cmd.current_dir(&dir);
            }
        }
        if cmd.spawn().is_ok() {
            return Ok(());
        }
    }

    Err("No terminal found (tried gtk-term, x-terminal-emulator, gnome-terminal, xterm)".into())
}

#[allow(dead_code)]
pub fn open_containing_folder(parent: Option<&impl IsA<gtk::Window>>, file: &gio::File) {
    if let Some(parent_file) = file.parent() {
        open_file_default(parent, &parent_file);
    }
}

pub fn show_error(parent: Option<&impl IsA<gtk::Window>>, title: &str, message: &str) {
    gtk_theme::present_message(parent, title, message);
}

pub fn confirm_dialog(
    parent: Option<&impl IsA<gtk::Window>>,
    title: &str,
    detail: &str,
    confirm_label: &str,
    cb: impl FnOnce(bool) + 'static,
) {
    gtk_theme::present_confirm(parent, title, detail, confirm_label, cb);
}

/// Unique destination path when copying/moving would overwrite.
pub fn uniquify_path(dest_dir: &Path, name: &str) -> PathBuf {
    let candidate = dest_dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..10_000 {
        let candidate = dest_dir.join(format!("{stem} ({i}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dest_dir.join(format!("{stem}-copy{ext}"))
}

pub fn content_type_description(info: &gio::FileInfo) -> String {
    if is_directory(info) {
        return "Folder".into();
    }
    let Some(ct) = effective_content_type(info) else {
        return "Unknown".into();
    };
    gio::content_type_get_description(&ct).to_string()
}

#[allow(dead_code)]
pub fn idle_add_local_once(f: impl FnOnce() + 'static) {
    glib::idle_add_local_once(f);
}
