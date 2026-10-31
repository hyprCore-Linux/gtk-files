//! Write a disk image to a removable USB drive.
//!
//! Internal disks and the disk that holds `/` are never offered. The copy
//! itself runs through pkexec so the user has to authorize the erase.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4 as gtk;
use gtk::gio;
use gtk::glib::{self, ControlFlow};
use gtk::prelude::*;

use crate::util::show_error;

const WRITE_SCRIPT: &str = r#"set -eu
name=$1
iso=$2
case "$name" in
  *[!a-z0-9]*|"") echo "refusing device name" >&2; exit 2 ;;
esac
if [ -e "/sys/class/block/$name/partition" ]; then
  echo "refusing to write to a partition" >&2
  exit 2
fi
disk=/dev/$name
[ -b "$disk" ] || { echo "not a block device: $disk" >&2; exit 2; }
[ -f "$iso" ] || { echo "image not found" >&2; exit 2; }
rootdev=$(/usr/bin/findmnt -rn -o SOURCE -T / 2>/dev/null || true)
case "$rootdev" in
  "$disk"|"$disk"[0-9]*|"$disk"p[0-9]*)
    echo "refusing to overwrite the system disk ($rootdev)" >&2
    exit 3
    ;;
esac
for part in "$disk" "$disk"[0-9] "$disk"[0-9][0-9] "$disk"p[0-9] "$disk"p[0-9][0-9]; do
  [ -b "$part" ] || continue
  mp=$(/usr/bin/findmnt -rn -o TARGET -S "$part" 2>/dev/null || true)
  case "$mp" in
    /|/boot|/boot/*|/home|/usr|/usr/*|/var|/var/*|/opt|/opt/*)
      echo "refusing to overwrite a disk mounted at $mp" >&2
      exit 3
      ;;
  esac
  if [ -n "$mp" ]; then
    /usr/bin/umount "$part" || {
      echo "could not unmount $part ($mp). Close anything using it and try again." >&2
      exit 4
    }
  fi
done
if [ -x /usr/bin/stdbuf ]; then
  /usr/bin/stdbuf -oL -eL /usr/bin/dd if="$iso" of="$disk" bs=4M conv=fsync status=progress
else
  /usr/bin/dd if="$iso" of="$disk" bs=4M conv=fsync status=progress
fi
/usr/bin/sync
"#;

#[derive(Clone)]
struct UsbDrive {
    name: String,
    size: String,
    model: String,
}

impl UsbDrive {
    fn label(&self) -> String {
        let dev = format!("/dev/{}", self.name);
        if self.model.is_empty() {
            format!("{dev}  —  {}", self.size)
        } else {
            format!("{}  —  {}  —  {dev}", self.model, self.size)
        }
    }
}

pub fn is_iso(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

pub fn show_write_dialog(parent: &gtk::Window, iso: &Path) {
    let iso = iso.to_path_buf();
    let iso_name = iso
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| iso.display().to_string());
    let iso_len = std::fs::metadata(&iso).map(|m| m.len()).unwrap_or(0);
    if iso_len == 0 {
        show_error(
            Some(parent),
            "Write to USB Drive",
            "That image is empty or could not be read.",
        );
        return;
    }

    let dialog = gtk::Window::builder()
        .title("Write to USB Drive")
        .modal(false)
        .transient_for(parent)
        .default_width(480)
        .build();
    gtk_theme::style_dialog(&dialog);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_start(16);
    root.set_margin_end(16);
    root.set_margin_top(16);
    root.set_margin_bottom(16);

    let heading = gtk::Label::new(Some(&format!(
        "{iso_name}  ·  {}",
        format_bytes(iso_len)
    )));
    heading.set_xalign(0.0);
    heading.set_wrap(true);
    heading.add_css_class("title-4");
    root.append(&heading);

    let hint = gtk::Label::new(Some(
        "Choose a USB drive. Everything on it will be erased. Internal disks are not listed.",
    ));
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    hint.add_css_class("dim-label");
    root.append(&hint);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("boxed-list");
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(140)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    root.append(&scroll);

    let empty = gtk::Label::new(Some("No USB drive found. Plug one in, then Refresh."));
    empty.set_xalign(0.0);
    empty.set_wrap(true);
    empty.add_css_class("dim-label");
    root.append(&empty);

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_text(Some("Waiting for permission…"));
    progress.set_visible(false);
    root.append(&progress);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_visible(false);
    root.append(&status);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let refresh = gtk_theme::labeled_button("view-refresh-symbolic", "Refresh");
    let cancel = gtk_theme::labeled_button(gtk_theme::icon_for_label("Cancel"), "Cancel");
    let write = gtk_theme::labeled_button("drive-removable-media-symbolic", "Write…");
    write.add_css_class("destructive-action");
    write.set_sensitive(false);
    buttons.append(&refresh);
    buttons.append(&cancel);
    buttons.append(&write);
    root.append(&buttons);
    dialog.set_child(Some(&root));

    let drives: Rc<RefCell<Vec<UsbDrive>>> = Rc::new(RefCell::new(Vec::new()));
    let busy = Rc::new(Cell::new(false));
    let cancelled = Rc::new(Cell::new(false));
    let writer_pid = Rc::new(Cell::new(None::<i32>));
    let active_write: Rc<RefCell<Option<(String, PathBuf)>>> = Rc::new(RefCell::new(None));

    {
        let list_for_select = list.clone();
        let empty_for_write = empty.clone();
        let drives_for_write = Rc::clone(&drives);
        let list = list.clone();
        let empty = empty.clone();
        let drives = Rc::clone(&drives);
        let write_for_refresh = write.clone();
        let refresh_list = move || {
            let found = list_usb_drives();
            *drives.borrow_mut() = found.clone();
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            for drive in &found {
                list.append(&drive_row(drive));
            }
            empty.set_visible(found.is_empty());
            write_for_refresh.set_sensitive(list.selected_row().is_some());
        };
        refresh_list();
        let refresh_list = Rc::new(refresh_list);

        {
            let refresh_list = Rc::clone(&refresh_list);
            refresh.connect_clicked(move |_| refresh_list());
        }
        {
            let write = write.clone();
            list_for_select.connect_row_selected(move |_, row| {
                write.set_sensitive(row.is_some());
            });
        }

        {
            let dialog = dialog.clone();
            let busy = Rc::clone(&busy);
            let cancelled = Rc::clone(&cancelled);
            let writer_pid = Rc::clone(&writer_pid);
            let active_write = Rc::clone(&active_write);
            let status = status.clone();
            cancel.connect_clicked(move |_| {
                if busy.get() {
                    cancelled.set(true);
                    let job = active_write.borrow().clone();
                    let pid = writer_pid.get();
                    status.set_text("Stopping the write… Approve the prompt to stop dd.");
                    if let Some((dev, iso)) = job {
                        request_stop(pid, &dev, &iso);
                    } else {
                        std::thread::spawn(move || stop_writer(pid, "", Path::new("")));
                    }
                } else {
                    dialog.close();
                }
            });
        }

        {
            let dialog = dialog.clone();
            let list = list_for_select.clone();
            let drives = Rc::clone(&drives_for_write);
            let busy = Rc::clone(&busy);
            let cancelled = Rc::clone(&cancelled);
            let writer_pid = Rc::clone(&writer_pid);
            let active_write = Rc::clone(&active_write);
            let progress = progress.clone();
            let status = status.clone();
            let hint = hint.clone();
            let scroll = scroll.clone();
            let empty = empty_for_write.clone();
            let refresh = refresh.clone();
            let write_btn = write.clone();
            let cancel = cancel.clone();
            let iso = iso.clone();
            let write_for_click = write_btn.clone();
            write_btn.connect_clicked(move |_| {
                let Some(name) = list
                    .selected_row()
                    .map(|row| row.widget_name().to_string())
                    .filter(|n| !n.is_empty())
                else {
                    return;
                };
                let summary = drives
                    .borrow()
                    .iter()
                    .find(|d| d.name == name)
                    .map(|d| d.label())
                    .unwrap_or_else(|| format!("/dev/{name}"));
                if let Err(err) = drive_is_safe(&name) {
                    show_error(Some(&dialog), "Write to USB Drive", &err);
                    return;
                }
                if image_is_on_disk(&iso, &name) {
                    show_error(
                        Some(&dialog),
                        "Write to USB Drive",
                        "That image is stored on the drive you selected. Copy it off the drive first.",
                    );
                    return;
                }
                let detail = format!(
                    "Write {iso_name} ({}) to {summary}?\n\nEverything on this drive will be erased. This cannot be undone.",
                    format_bytes(iso_len)
                );
                let dialog_c = dialog.clone();
                let name_c = name.clone();
                let iso_c = iso.clone();
                let busy = Rc::clone(&busy);
                let cancelled = Rc::clone(&cancelled);
                let writer_pid = Rc::clone(&writer_pid);
                let progress = progress.clone();
                let status = status.clone();
                let hint = hint.clone();
                let scroll = scroll.clone();
                let empty = empty.clone();
                let refresh = refresh.clone();
                let write = write_for_click.clone();
                let cancel = cancel.clone();
                let active_write = Rc::clone(&active_write);
                let confirm = gtk::AlertDialog::builder()
                    .modal(true)
                    .message("Erase USB drive?")
                    .detail(&detail)
                    .buttons(["Cancel", "Erase and Write"])
                    .default_button(0)
                    .cancel_button(0)
                    .build();
                confirm.choose(
                    Some(&dialog),
                    None::<&gio::Cancellable>,
                    move |res| {
                        if !matches!(res, Ok(1)) {
                            return;
                        }
                        if let Err(err) = drive_is_safe(&name_c) {
                            show_error(Some(&dialog_c), "Write to USB Drive", &err);
                            return;
                        }
                        begin_write(
                            &dialog_c,
                            &iso_c,
                            &name_c,
                            iso_len,
                            &busy,
                            &cancelled,
                            &writer_pid,
                            &active_write,
                            &progress,
                            &status,
                            &hint,
                            &scroll,
                            &empty,
                            &refresh,
                            &write,
                            &cancel,
                        );
                    },
                );
            });
        }
    }

    let allow_close = Rc::new(Cell::new(false));
    let prompting = Rc::new(Cell::new(false));
    {
        let busy = Rc::clone(&busy);
        let cancelled = Rc::clone(&cancelled);
        let writer_pid = Rc::clone(&writer_pid);
        let active_write = Rc::clone(&active_write);
        let status = status.clone();
        let allow_close = Rc::clone(&allow_close);
        let prompting = Rc::clone(&prompting);
        dialog.connect_close_request(move |dialog| {
            if !busy.get() || allow_close.get() {
                return glib::Propagation::Proceed;
            }
            if prompting.get() {
                return glib::Propagation::Stop;
            }
            prompting.set(true);
            let dialog_close = dialog.clone();
            let cancelled = Rc::clone(&cancelled);
            let writer_pid = Rc::clone(&writer_pid);
            let active_write = Rc::clone(&active_write);
            let status = status.clone();
            let allow_close = Rc::clone(&allow_close);
            let prompting = Rc::clone(&prompting);
            ask_stop_on_close(&dialog, move |stop| {
                prompting.set(false);
                if !stop {
                    return;
                }
                cancelled.set(true);
                status.set_text("Stopping the write… Approve the prompt to stop dd.");
                if let Some((dev, iso)) = active_write.borrow().clone() {
                    request_stop(writer_pid.get(), &dev, &iso);
                }
                allow_close.set(true);
                dialog_close.close();
            });
            glib::Propagation::Stop
        });
    }

    dialog.present();
}

fn begin_write(
    dialog: &gtk::Window,
    iso: &Path,
    name: &str,
    iso_len: u64,
    busy: &Rc<Cell<bool>>,
    cancelled: &Rc<Cell<bool>>,
    writer_pid: &Rc<Cell<Option<i32>>>,
    active_write: &Rc<RefCell<Option<(String, PathBuf)>>>,
    progress: &gtk::ProgressBar,
    status: &gtk::Label,
    hint: &gtk::Label,
    scroll: &gtk::ScrolledWindow,
    empty: &gtk::Label,
    refresh: &gtk::Button,
    write: &gtk::Button,
    cancel: &gtk::Button,
) {
    if !command_exists("pkexec") {
        show_error(
            Some(dialog),
            "Write to USB Drive",
            "pkexec was not found. Install polkit so the image can be written as root.",
        );
        return;
    }

    cancelled.set(false);
    *active_write.borrow_mut() = Some((name.to_string(), iso.to_path_buf()));
    busy.set(true);
    hint.set_visible(false);
    scroll.set_visible(false);
    empty.set_visible(false);
    refresh.set_visible(false);
    write.set_visible(false);
    progress.set_visible(true);
    progress.set_fraction(0.0);
    progress.set_text(Some("Writing…"));
    status.set_visible(true);
    status.set_text(&format!(
        "Writing the image to /dev/{name}. Do not unplug the drive."
    ));
    set_button_caption(cancel, gtk_theme::icon_for_label("Cancel"), "Cancel");
    let bytes_at_start = disk_bytes_written(name).unwrap_or(0);

    let mut cmd = Command::new("pkexec");
    cmd.arg("/bin/sh")
        .arg("-c")
        .arg(WRITE_SCRIPT)
        .arg("_")
        .arg(name)
        .arg(iso)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            busy.set(false);
            *active_write.borrow_mut() = None;
            show_error(
                Some(dialog),
                "Write to USB Drive",
                &format!("Could not start pkexec: {err}"),
            );
            return;
        }
    };
    let pid = child.id() as i32;
    writer_pid.set(Some(pid));
    register_live(name, iso, pid);
    let stderr = child.stderr.take();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut notes: Vec<String> = Vec::new();
        if let Some(stderr) = stderr {
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = [0u8; 4096];
            let mut pending = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &byte in &buf[..n] {
                            if byte == b'\r' || byte == b'\n' {
                                if pending.is_empty() {
                                    continue;
                                }
                                let line = String::from_utf8_lossy(&pending).trim().to_string();
                                pending.clear();
                                if line.is_empty() {
                                    continue;
                                }
                                if let Some(copied) = parse_dd_bytes(&line) {
                                    let _ = tx.send(WriteMsg::Progress { copied });
                                } else {
                                    notes.push(line);
                                }
                            } else {
                                pending.push(byte);
                            }
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(err) => {
                        notes.push(format!("could not read progress: {err}"));
                        break;
                    }
                }
            }
        }
        let status = child.wait();
        let _ = tx.send(WriteMsg::PkexecExited { status, notes });
    });

    let busy = Rc::clone(busy);
    let cancelled = Rc::clone(cancelled);
    let writer_pid = Rc::clone(writer_pid);
    let progress = progress.clone();
    let status_label = status.clone();
    let cancel = cancel.clone();
    let pulse = Rc::new(Cell::new(true));
    let dev_name = name.to_string();
    let iso_watch = iso.to_path_buf();
    let copied_bytes = Rc::new(Cell::new(0u64));
    let seen_dd = Rc::new(Cell::new(false));
    let started = Instant::now();
    let pkexec_end: Rc<RefCell<Option<(Option<i32>, Vec<String>)>>> = Rc::new(RefCell::new(None));
    let active_write = Rc::clone(active_write);
    glib::timeout_add_local(Duration::from_millis(200), move || {
        if let Some(now) = disk_bytes_written(&dev_name) {
            let copied = now.saturating_sub(bytes_at_start);
            if copied > copied_bytes.get() {
                copied_bytes.set(copied);
                show_write_progress(&progress, &status_label, &dev_name, copied, iso_len);
                pulse.set(false);
            }
        }
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WriteMsg::Progress { copied } => {
                    if copied > copied_bytes.get() {
                        copied_bytes.set(copied);
                        show_write_progress(
                            &progress,
                            &status_label,
                            &dev_name,
                            copied,
                            iso_len,
                        );
                        pulse.set(false);
                    }
                }
                WriteMsg::PkexecExited { status, notes } => {
                    let code = status.ok().and_then(|s| s.code());
                    *pkexec_end.borrow_mut() = Some((code, notes));
                }
            }
        }

        let dd_running = !dd_pids(&iso_watch, &dev_name).is_empty();
        if dd_running {
            seen_dd.set(true);
        }
        let pkexec_done = pkexec_end.borrow().is_some();
        let code = pkexec_end.borrow().as_ref().and_then(|(code, _)| *code);
        let auth_failed = matches!(code, Some(126) | Some(127)) && !seen_dd.get();
        let dd_finished = seen_dd.get() && !dd_running;
        if live_stop_sent(&dev_name) {
            cancelled.set(true);
        }
        let never_started = pkexec_done
            && !seen_dd.get()
            && !auth_failed
            && started.elapsed() >= Duration::from_secs(4);
        if !(auth_failed || dd_finished || never_started) {
            if pulse.get() {
                progress.pulse();
            }
            return ControlFlow::Continue;
        }

        pulse.set(false);
        busy.set(false);
        writer_pid.set(None);
        *active_write.borrow_mut() = None;
        forget_live(&dev_name);
        set_button_caption(&cancel, gtk_theme::icon_for_label("Close"), "Close");
        if let Some(now) = disk_bytes_written(&dev_name) {
            let copied = now.saturating_sub(bytes_at_start);
            if copied > copied_bytes.get() {
                copied_bytes.set(copied);
            }
        }
        let copied = copied_bytes.get();
        let notes = pkexec_end
            .borrow()
            .as_ref()
            .map(|(_, notes)| notes.clone())
            .unwrap_or_default();
        if cancelled.get() {
            progress.set_text(Some("Stopped"));
            status_label.set_text(
                "The write was stopped. This drive is incomplete and will not boot until you write it again.",
            );
        } else if auth_failed {
            progress.set_text(Some("Not written"));
            status_label.set_text("Permission was denied, so nothing was written.");
        } else if write_looks_complete(copied, iso_len) {
            progress.set_fraction(1.0);
            progress.set_text(Some("Finished"));
            status_label.set_text("Finished. The image was written. You can remove the USB drive.");
        } else if never_started {
            progress.set_text(Some("Failed"));
            let detail = notes
                .iter()
                .rev()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            if detail.trim().is_empty() {
                status_label.set_text("The write did not start.");
            } else {
                status_label.set_text(detail.trim());
            }
        } else {
            progress.set_text(Some("Failed"));
            status_label.set_text(&format!(
                "The write stopped early ({} of {}).",
                format_bytes(copied),
                format_bytes(iso_len)
            ));
        }
        ControlFlow::Break
    });
}

enum WriteMsg {
    Progress { copied: u64 },
    PkexecExited {
        status: std::io::Result<std::process::ExitStatus>,
        notes: Vec<String>,
    },
}

fn disk_bytes_written(name: &str) -> Option<u64> {
    if !device_name_ok(name) {
        return None;
    }
    let text = std::fs::read_to_string(format!("/sys/block/{name}/stat")).ok()?;
    // Field 7 is sectors written (512-byte sectors) since boot.
    let sectors: u64 = text.split_whitespace().nth(6)?.parse().ok()?;
    Some(sectors.saturating_mul(512))
}

fn show_write_progress(
    progress: &gtk::ProgressBar,
    status: &gtk::Label,
    name: &str,
    copied: u64,
    iso_len: u64,
) {
    let shown = if iso_len == 0 { copied } else { copied.min(iso_len) };
    let frac = if iso_len == 0 {
        0.0
    } else {
        (shown as f64 / iso_len as f64).clamp(0.0, 0.99)
    };
    let pct = (frac * 100.0).round() as u32;
    progress.set_fraction(frac);
    progress.set_text(Some(&format!(
        "{pct}%  ·  {} of {}",
        format_bytes(shown),
        format_bytes(iso_len)
    )));
    status.set_text(&format!(
        "Writing the image to /dev/{name}. Do not unplug the drive."
    ));
}

fn write_looks_complete(copied: u64, iso_len: u64) -> bool {
    if iso_len == 0 {
        return false;
    }
    // The last 4 MiB block can land in the disk stats slightly after dd exits.
    copied.saturating_add(4 * 1024 * 1024) >= iso_len
}

fn drive_row(drive: &UsbDrive) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    let label = gtk::Label::new(Some(&drive.label()));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_margin_top(8);
    label.set_margin_bottom(8);
    label.set_margin_start(10);
    label.set_margin_end(10);
    row.set_child(Some(&label));
    row.set_widget_name(&drive.name);
    row
}

fn list_usb_drives() -> Vec<UsbDrive> {
    let output = Command::new("lsblk")
        .args(["-d", "-P", "-o", "NAME,TRAN,RM,SIZE,MODEL,TYPE"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut drives = Vec::new();
    for line in text.lines() {
        let fields = parse_lsblk_line(line);
        let name = fields.get("NAME").map(|s| s.as_str()).unwrap_or("");
        let kind = fields.get("TYPE").map(|s| s.as_str()).unwrap_or("");
        let tran = fields.get("TRAN").map(|s| s.as_str()).unwrap_or("");
        let rm = fields.get("RM").map(|s| s.as_str()).unwrap_or("");
        if kind != "disk" || !device_name_ok(name) {
            continue;
        }
        if tran != "usb" && rm != "1" {
            continue;
        }
        if disk_holds_system(name) {
            continue;
        }
        drives.push(UsbDrive {
            name: name.to_string(),
            size: fields.get("SIZE").cloned().unwrap_or_default(),
            model: fields
                .get("MODEL")
                .map(|s| s.trim().to_string())
                .unwrap_or_default(),
        });
    }
    drives
}

fn drive_is_safe(name: &str) -> Result<(), String> {
    if !device_name_ok(name) {
        return Err("That device name is not a whole USB disk.".into());
    }
    if list_usb_drives().iter().any(|d| d.name == name) {
        Ok(())
    } else {
        Err("That USB drive is no longer available. Refresh the list and choose it again.".into())
    }
}

fn device_name_ok(name: &str) -> bool {
    if name.is_empty()
        || name.starts_with("loop")
        || name.starts_with("zram")
        || name.starts_with("dm-")
        || name.starts_with("sr")
        || name.starts_with("fd")
    {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn disk_holds_system(name: &str) -> bool {
    let output = Command::new("lsblk")
        .args(["-ln", "-o", "MOUNTPOINT", &format!("/dev/{name}")])
        .output();
    let Ok(output) = output else {
        return true;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for mount in text.lines() {
        let mount = mount.trim();
        if mount.is_empty() {
            continue;
        }
        if mount == "/"
            || mount == "/boot"
            || mount == "/home"
            || mount == "/usr"
            || mount == "/var"
            || mount == "/opt"
            || mount.starts_with("/boot/")
            || mount.starts_with("/usr/")
            || mount.starts_with("/var/")
            || mount.starts_with("/opt/")
        {
            return true;
        }
    }
    false
}

fn image_is_on_disk(iso: &Path, name: &str) -> bool {
    let output = Command::new("findmnt")
        .args(["-no", "SOURCE", "-T"])
        .arg(iso)
        .output();
    let Ok(output) = output else {
        return false;
    };
    let source = String::from_utf8_lossy(&output.stdout);
    let source = source.trim();
    source == format!("/dev/{name}")
        || source.starts_with(&format!("/dev/{name}p"))
        || source_is_partition(source, name)
}

fn source_is_partition(source: &str, name: &str) -> bool {
    let Some(rest) = source.strip_prefix(&format!("/dev/{name}")) else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

fn parse_lsblk_line(line: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key = line[key_start..i].to_string();
        i += 1;
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            map.insert(key, line[val_start..i].to_string());
            if i < bytes.len() {
                i += 1;
            }
        }
    }
    map
}

fn parse_dd_bytes(line: &str) -> Option<u64> {
    let line = line.trim();
    if !line.contains("bytes") {
        return None;
    }
    let num: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn set_button_caption(button: &gtk::Button, icon: &str, text: &str) {
    button.set_child(Some(&gtk_theme::icon_label_box(icon, text)));
}

fn command_exists(bin: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {bin} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn dd_pids(iso: &Path, dev: &str) -> Vec<i32> {
    if dev.is_empty() || !device_name_ok(dev) {
        return Vec::new();
    }
    let if_exact = format!("if={}", iso.display());
    let if_name = iso.file_name().and_then(|name| name.to_str());
    let of_arg = format!("of=/dev/{dev}");
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for ent in entries.flatten() {
        let Ok(pid) = ent.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Some(args) = proc_args(pid) else {
            continue;
        };
        let Some(argv0) = args.first() else {
            continue;
        };
        let is_dd = argv0 == "dd" || argv0.ends_with("/dd");
        if !is_dd || !args.iter().any(|arg| arg == &of_arg) {
            continue;
        }
        let if_ok = args.iter().any(|arg| {
            if arg == &if_exact {
                return true;
            }
            let Some(path) = arg.strip_prefix("if=") else {
                return false;
            };
            if_name.is_some_and(|name| {
                Path::new(path).file_name().and_then(|n| n.to_str()) == Some(name)
            })
        });
        if if_ok {
            pids.push(pid);
        }
    }
    pids
}

fn proc_args(pid: i32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(
        bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

fn writer_shell_pids(dd: &[i32]) -> Vec<i32> {
    let mut out = Vec::new();
    for pid in dd {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let Some(ppid) = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|rest| rest.trim().parse::<i32>().ok())
        }) else {
            continue;
        };
        if ppid <= 1 {
            continue;
        }
        let comm = std::fs::read_to_string(format!("/proc/{ppid}/comm")).unwrap_or_default();
        if comm.trim() == "sh" || comm.trim() == "bash" {
            out.push(ppid);
        }
    }
    out
}

struct LiveWrite {
    dev: String,
    iso: PathBuf,
    pid: Option<i32>,
    stop_sent: bool,
}

thread_local! {
    static LIVE_WRITES: RefCell<Vec<LiveWrite>> = RefCell::new(Vec::new());
}

fn register_live(dev: &str, iso: &Path, pid: i32) {
    LIVE_WRITES.with(|live| {
        live.borrow_mut().push(LiveWrite {
            dev: dev.to_string(),
            iso: iso.to_path_buf(),
            pid: Some(pid),
            stop_sent: false,
        });
    });
}

fn forget_live(dev: &str) {
    LIVE_WRITES.with(|live| {
        live.borrow_mut().retain(|job| job.dev != dev);
    });
}

fn write_blocks_close() -> bool {
    LIVE_WRITES.with(|live| live.borrow().iter().any(|job| !job.stop_sent))
}

fn live_stop_sent(dev: &str) -> bool {
    LIVE_WRITES.with(|live| {
        live.borrow()
            .iter()
            .any(|job| job.dev == dev && job.stop_sent)
    })
}

fn request_stop(pid: Option<i32>, dev: &str, iso: &Path) {
    LIVE_WRITES.with(|live| {
        for job in live.borrow_mut().iter_mut().filter(|job| job.dev == dev) {
            job.stop_sent = true;
        }
    });
    let dev = dev.to_string();
    let iso = iso.to_path_buf();
    std::thread::spawn(move || stop_writer(pid, &dev, &iso));
}

fn stop_outstanding_writes() -> bool {
    let jobs = LIVE_WRITES.with(|live| {
        let mut live = live.borrow_mut();
        let jobs: Vec<_> = live
            .iter()
            .filter(|job| !job.stop_sent)
            .map(|job| (job.pid, job.dev.clone(), job.iso.clone()))
            .collect();
        for job in live.iter_mut() {
            job.stop_sent = true;
        }
        jobs
    });
    let mut stopped = true;
    for (pid, dev, iso) in jobs {
        if stop_writer(pid, &dev, &iso) {
            continue;
        }
        stopped = false;
        LIVE_WRITES.with(|live| {
            for job in live.borrow_mut().iter_mut().filter(|job| job.dev == dev) {
                job.stop_sent = false;
            }
        });
    }
    stopped
}

fn ask_stop_on_close(parent: &gtk::Window, on_answer: impl Fn(bool) + 'static) {
    let confirm = gtk::AlertDialog::builder()
        .modal(true)
        .message("Stop the USB write?")
        .detail("A write is still running. Closing will stop dd and leave this drive incomplete.")
        .buttons(["Keep writing", "Stop and close"])
        .default_button(0)
        .cancel_button(0)
        .build();
    confirm.choose(
        Some(parent),
        None::<&gio::Cancellable>,
        move |res| on_answer(matches!(res, Ok(1))),
    );
}

thread_local! {
    static ALLOW_MAIN_CLOSE: Cell<bool> = const { Cell::new(false) };
    static MAIN_CLOSE_PROMPT: Cell<bool> = const { Cell::new(false) };
}

/// Called from the file manager window's close handler. That handler runs
/// before any later close-request callback, so the warning has to live here.
pub fn intercept_main_window_close(window: &gtk::Window) -> glib::Propagation {
    if ALLOW_MAIN_CLOSE.with(Cell::get) {
        ALLOW_MAIN_CLOSE.with(|flag| flag.set(false));
        return glib::Propagation::Proceed;
    }
    if !write_blocks_close() {
        return glib::Propagation::Proceed;
    }
    if MAIN_CLOSE_PROMPT.with(Cell::get) {
        return glib::Propagation::Stop;
    }
    MAIN_CLOSE_PROMPT.with(|flag| flag.set(true));
    let window_close = window.clone();
    window.present();
    ask_stop_on_close(window, move |stop| {
        MAIN_CLOSE_PROMPT.with(|flag| flag.set(false));
        if !stop {
            return;
        }
        // Stay alive until the root dd is gone. Quitting first drops the
        // pkexec kill, and the write keeps running under init.
        if !stop_outstanding_writes() {
            show_error(
                Some(&window_close),
                "USB write still running",
                "dd is still writing the drive. Approve the permission prompt to stop it, then close again.",
            );
            return;
        }
        ALLOW_MAIN_CLOSE.with(|flag| flag.set(true));
        window_close.close();
    });
    glib::Propagation::Stop
}

const STOP_SCRIPT: &str = r#"set -u
for p in "$@"; do
  case "$p" in
    ''|*[!0-9]*) echo "refusing pid" >&2; exit 2 ;;
  esac
done
/usr/bin/kill -TERM "$@" 2>/dev/null || /bin/kill -TERM "$@" 2>/dev/null || true
i=0
while [ "$i" -lt 40 ]; do
  alive=0
  for p in "$@"; do
    if /usr/bin/kill -0 "$p" 2>/dev/null || /bin/kill -0 "$p" 2>/dev/null; then
      alive=1
    fi
  done
  if [ "$alive" = 0 ]; then
    exit 0
  fi
  i=$((i + 1))
  /bin/sleep 0.25
done
/usr/bin/kill -KILL "$@" 2>/dev/null || /bin/kill -KILL "$@" 2>/dev/null || true
/bin/sleep 0.5
for p in "$@"; do
  if /usr/bin/kill -0 "$p" 2>/dev/null || /bin/kill -0 "$p" 2>/dev/null; then
    exit 1
  fi
done
exit 0
"#;

/// Stop the root `dd` and its shell. Returns true only when those processes are gone.
/// Blocks through the pkexec prompt so the app cannot exit first and abandon the kill.
fn stop_writer(local_pid: Option<i32>, dev: &str, iso: &Path) -> bool {
    let dds = dd_pids(iso, dev);
    let mut pids = dds.clone();
    pids.extend(writer_shell_pids(&dds));
    pids.retain(|pid| *pid > 1);
    pids.sort_unstable();
    pids.dedup();

    // The unprivileged pkexec client is not the parent of the root shell.
    // Killing it does not stop dd, so do it only after the root pids are known.
    if let Some(pid) = local_pid.filter(|p| *p > 1 && !pids.contains(p)) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    if pids.is_empty() {
        return dd_pids(iso, dev).is_empty();
    }

    let mut cmd = Command::new("pkexec");
    cmd.arg("/bin/sh")
        .arg("-c")
        .arg(STOP_SCRIPT)
        .arg("_");
    for pid in &pids {
        cmd.arg(pid.to_string());
    }
    let stopped = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    stopped && dd_pids(iso, dev).is_empty()
}
