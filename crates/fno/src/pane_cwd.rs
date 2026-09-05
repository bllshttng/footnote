//! What directory was a pane in? (x-5baf)
//!
//! A shell's own cwd, not the pane's spawn cwd: every tab is born at the
//! squad root (`create_tab_in`), so the spawn cwd tells two tabs apart in no
//! case. Read live from the kernel when the child pid still resolves; a dead
//! pid degrades to the caller's own spawn-cwd fallback rather than failing.

/// macOS: `libc::proc_pidinfo` + `PROC_PIDVNODEPATHINFO`.
#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if written != size {
        return None;
    }
    // `vip_path` is `[[c_char; 32]; 32]`, a libc workaround for
    // `[c_char; MAXPATHLEN]` (1024) that flattens identically since a 2D
    // fixed array has no inter-row padding.
    let bytes: &[libc::c_char] = unsafe {
        std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr().cast::<libc::c_char>(),
            32 * 32,
        )
    };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    let path_bytes: Vec<u8> = bytes[..end].iter().map(|&b| b as u8).collect();
    String::from_utf8(path_bytes).ok()
}

/// Linux: `/proc/<pid>/cwd`.
#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_cwd(_pid: u32) -> Option<String> {
    None
}

/// A pane's cwd at capture: the live process cwd when `child_pid` still
/// resolves, else `spawn_cwd` (the pane's own recorded spawn cwd).
pub fn live_or_spawn(child_pid: Option<u32>, spawn_cwd: &str) -> String {
    child_pid
        .and_then(process_cwd)
        .unwrap_or_else(|| spawn_cwd.to_string())
}

impl crate::proto::LayoutSlot {
    /// A slot with no recorded cwd (every site but capture itself, x-5baf).
    pub fn new(name: String, binding: crate::proto::LayoutBinding) -> Self {
        Self {
            name,
            binding,
            cwd: None,
        }
    }
}

/// Every surviving leaf's cwd, filled from `lookup` where a pane resolves and
/// no cwd is recorded yet (a portal seat pruned out of `leaves` never calls
/// `lookup`, so it never costs a syscall for a cwd nothing reads).
pub fn fill_leaf_cwds<'a>(
    leaves: impl IntoIterator<Item = u64>,
    lookup: impl Fn(u64) -> Option<(Option<u32>, &'a str)>,
    into: &mut std::collections::HashMap<u64, String>,
) {
    for pane in leaves {
        if into.contains_key(&pane) {
            continue;
        }
        if let Some((child_pid, spawn_cwd)) = lookup(pane) {
            into.insert(pane, live_or_spawn(child_pid, spawn_cwd));
        }
    }
}

/// Only a genuine Shell slot's cwd steers restore; a dead worker's substitute
/// shell still spawns at the squad root. Returns the spawn cwd plus a notice
/// to raise when the stored cwd is gone.
fn shell_restore_cwd(
    is_shell: bool,
    slot_cwd: Option<&str>,
    cwd0: &str,
    tab_name: &str,
    slot_name: &str,
) -> (String, Option<String>) {
    let stored = is_shell.then_some(slot_cwd).flatten();
    let (spawn_cwd, gone) =
        crate::server::restore_member_cwd(stored, cwd0, |p| std::path::Path::new(p).is_dir());
    let notice = gone.map(|g| {
        format!(
            "restore: tab {tab_name}: {slot_name}'s directory {g} is gone; restored at {spawn_cwd} instead"
        )
    });
    (spawn_cwd, notice)
}

impl crate::server::Core {
    /// (x-5baf) A slot pane that could not be resolved: mint a shell,
    /// steered by the slot's own captured cwd for a genuine Shell slot
    /// whose directory still exists.
    pub(crate) fn restore_shell_slot(
        &mut self,
        slot: &crate::proto::LayoutSlot,
        rows: u16,
        cols: u16,
        cwd0: &str,
        tab_name: &str,
    ) -> Option<u64> {
        let is_shell = matches!(slot.binding, crate::proto::LayoutBinding::Shell);
        let (spawn_cwd, notice) =
            shell_restore_cwd(is_shell, slot.cwd.as_deref(), cwd0, tab_name, &slot.name);
        if let Some(notice) = notice {
            self.notice_all(notice);
        }
        match self.spawn_pane(rows, cols, &spawn_cwd) {
            Ok(p) => Some(p),
            Err(e) => {
                self.notice_all(format!(
                    "restore: tab {tab_name}: could not open shell: {e}"
                ));
                None
            }
        }
    }
}
