//! Host→guest file-paste helpers for an RDP clipboard (MS-RDPECLIP) server
//! that stages Windows files into a local directory.
//!
//! - [`plan_staged_tree`] turns a FileGroupDescriptorW listing into a safe
//!   relative tree: every entry gets a sanitized relative path (no `..`,
//!   no absolute paths, no `/` smuggled inside a component), a
//!   file-or-directory verdict, and a top-level flag. Only top-level
//!   entries belong in the `text/uri-list` handed to the local file
//!   manager — announcing nested entries as well makes it paste them twice
//!   (once inside their directory, once flattened beside it).
//! - [`expand_outgoing`] is the guest→host direction: copied local paths
//!   expanded into the parents-first entry list an offer needs, so a
//!   copied directory arrives with its contents rather than empty.
//! - [`take_pending_file_paste`] picks the local paste request that a
//!   remote file list answers out of a FIFO of pending clipboard requests.
//! - [`FormatListDedup`] drops the duplicate Format List a single host copy
//!   can arrive as, so the copy is handled once.
//!
//! Windows lists a copied directory as its own descriptor
//! (`FILE_ATTRIBUTE_DIRECTORY`, size 0) followed by its contents, each
//! named relative to the copied collection with `\` separators. Some
//! decoders pass the attribute through; others keep only the name, size
//! and relative directory. When the attribute is unknown, an entry is a
//! directory exactly when another entry lies beneath it; an *empty*
//! directory is then indistinguishable from an empty file and is staged as
//! one.
//!
//! Pure `std`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Linux `NAME_MAX`: the longest path component, in bytes.
pub const MAX_COMPONENT_BYTES: usize = 255;

/// MIME types under which a local application asks for a file paste.
pub const FILE_PASTE_MIMES: [&str; 2] = ["text/uri-list", "x-special/gnome-copied-files"];

/// One FileGroupDescriptorW entry as a decoder reports it.
#[derive(Debug, Clone, Copy)]
pub struct RemoteEntry<'a> {
    /// Directory of the entry within the copied collection, `\`-separated;
    /// `None` (or empty) for a root-level entry.
    pub relative_dir: Option<&'a str>,
    /// Entry name. A leaf, or a full `\`-separated path for decoders that
    /// do not split it.
    pub name: &'a str,
    /// `FILE_ATTRIBUTE_DIRECTORY`, when the decoder kept the attributes.
    pub is_directory: Option<bool>,
}

/// Where one remote entry lands, relative to the staging directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedEntry {
    /// Sanitized relative path, never empty, never escaping the staging
    /// directory.
    pub path: PathBuf,
    pub is_directory: bool,
    /// The entry sits directly in the copied collection; announce it.
    pub top_level: bool,
}

/// Make one path component safe for a Linux filesystem: `/` and NUL become
/// `_`, control characters are dropped, `.`/`..`/empty become `_`, and the
/// result is cut to [`MAX_COMPONENT_BYTES`] on a character boundary.
pub fn sanitize_component(component: &str) -> String {
    let mut out: String = component
        .chars()
        .filter(|c| *c == '\0' || !c.is_control())
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        return "_".to_string();
    }
    if out.len() > MAX_COMPONENT_BYTES {
        let mut cut = MAX_COMPONENT_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// Sanitized components of `relative_dir` followed by `name`. Empty
/// segments (doubled or trailing separators) are skipped rather than
/// turned into `_`, so `a\\b` and `a\b` agree.
fn components(entry: &RemoteEntry<'_>) -> Vec<String> {
    entry
        .relative_dir
        .unwrap_or("")
        .split('\\')
        .chain(entry.name.split('\\'))
        .filter(|s| !s.is_empty())
        .map(sanitize_component)
        .collect()
}

/// Plan the local tree for a remote file list. The result has one
/// [`StagedEntry`] per input entry, in input order (so list indices for
/// file-contents requests carry over unchanged).
pub fn plan_staged_tree(entries: &[RemoteEntry<'_>]) -> Vec<StagedEntry> {
    let paths: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            let c = components(e);
            if c.is_empty() { vec!["_".to_string()] } else { c }
        })
        .collect();

    entries
        .iter()
        .zip(&paths)
        .map(|(entry, path)| {
            let is_directory = entry.is_directory.unwrap_or_else(|| {
                paths
                    .iter()
                    .any(|other| other.len() > path.len() && other.starts_with(path))
            });
            StagedEntry {
                path: path.iter().collect(),
                is_directory,
                top_level: path.len() == 1,
            }
        })
        .collect()
}

/// Longest FileGroupDescriptorW `cFileName` (`relative_dir\name`), in UTF-16
/// code units, leaving room for the terminator in the 260-unit field.
pub const MAX_WIRE_NAME_UNITS: usize = 259;

/// One local entry to offer in a FileGroupDescriptorW list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingEntry {
    /// Local path to serve contents from.
    pub path: PathBuf,
    /// Directory within the copied collection, `\`-separated; `None` for a
    /// copied item itself.
    pub relative_dir: Option<String>,
    /// Leaf name.
    pub name: String,
    pub is_directory: bool,
    /// Size in bytes (0 for directories).
    pub size: u64,
}

/// Expand copied local paths into the flat list a FileGroupDescriptorW offer
/// needs: each copied item, and for a copied directory everything beneath
/// it, parents before children (Windows creates entries in list order).
/// Offering only the directory itself makes the receiver create it empty.
///
/// Symlinked directories below a root are skipped (loop safety); symlinked
/// files are offered with their target's contents. Unreadable entries,
/// entries whose wire name would exceed [`MAX_WIRE_NAME_UNITS`], and
/// anything past `max_entries` are skipped; the second value counts them.
pub fn expand_outgoing(roots: &[PathBuf], max_entries: usize) -> (Vec<OutgoingEntry>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0;
    for root in roots {
        let Some(name) = root.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            skipped += 1;
            continue;
        };
        let Ok(meta) = std::fs::metadata(root) else {
            skipped += 1;
            continue;
        };
        walk(root, None, name, &meta, max_entries, &mut out, &mut skipped);
    }
    (out, skipped)
}

fn walk(
    path: &std::path::Path,
    relative_dir: Option<String>,
    name: String,
    meta: &std::fs::Metadata,
    max_entries: usize,
    out: &mut Vec<OutgoingEntry>,
    skipped: &mut usize,
) {
    let wire_units = relative_dir
        .as_deref()
        .map_or(0, |d| d.encode_utf16().count() + 1)
        + name.encode_utf16().count();
    if name.is_empty() || wire_units > MAX_WIRE_NAME_UNITS || out.len() >= max_entries {
        // A directory's contents go with it: their wire names only grow.
        *skipped += 1;
        return;
    }
    let is_directory = meta.is_dir();
    out.push(OutgoingEntry {
        path: path.to_path_buf(),
        relative_dir: relative_dir.clone(),
        name: name.clone(),
        is_directory,
        size: if is_directory { 0 } else { meta.len() },
    });
    if !is_directory {
        return;
    }

    let Ok(read_dir) = std::fs::read_dir(path) else {
        return;
    };
    let mut children: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
    children.sort_by_key(|e| e.file_name());
    let child_dir = match relative_dir {
        Some(d) => format!("{d}\\{name}"),
        None => name,
    };
    for child in children {
        let child_path = child.path();
        let Ok(link_meta) = std::fs::symlink_metadata(&child_path) else {
            *skipped += 1;
            continue;
        };
        let meta = if link_meta.file_type().is_symlink() {
            match std::fs::metadata(&child_path) {
                Ok(m) if !m.is_dir() => m,
                _ => {
                    *skipped += 1;
                    continue;
                }
            }
        } else {
            link_meta
        };
        let child_name = child.file_name().to_string_lossy().into_owned();
        walk(
            &child_path,
            Some(child_dir.clone()),
            child_name,
            &meta,
            max_entries,
            out,
            skipped,
        );
    }
}

/// Remove and return the oldest pending request that a remote file list
/// answers: the first entry asking for one of [`FILE_PASTE_MIMES`] whose
/// serial is not `skip_serial` (a sentinel for requests the server issued
/// itself, such as an eager prefetch). Entries are `(serial, mime, extra)`.
pub fn take_pending_file_paste<T>(
    pending: &mut VecDeque<(u32, String, T)>,
    skip_serial: u32,
) -> Option<(u32, String, T)> {
    pending
        .iter()
        .position(|(serial, mime, _)| {
            *serial != skip_serial && FILE_PASTE_MIMES.contains(&mime.as_str())
        })
        .and_then(|i| pending.remove(i))
}

/// Drops a remote Format List that repeats the previous one within a short
/// window.
///
/// One copy on the Windows host can reach the server as two identical
/// Format List PDUs microseconds apart (Explorer updates the clipboard
/// twice for a single copy, and vmconnect forwards each update). Handling
/// both takes local selection ownership twice, so every local paste source
/// fetches the FileGroupDescriptorW twice concurrently; vmconnect then fails
/// one of the requests and the paste breaks. A list equal to the previous
/// one (same format ids and names, same order) that arrives within the
/// window is that echo, not a new copy.
///
/// The window restarts on every list, admitted or not, so a burst of echoes
/// collapses into the first. Call [`FormatListDedup::reset`] when the
/// session ends.
#[derive(Debug)]
pub struct FormatListDedup {
    window: Duration,
    last: Option<(FormatList, Instant)>,
}

/// `(format id, name)` pairs in wire order.
type FormatList = Vec<(u32, Option<String>)>;

impl FormatListDedup {
    /// Observed echoes arrive within ~0.1 ms; a person cannot copy twice in
    /// 250 ms.
    pub const DEFAULT_WINDOW: Duration = Duration::from_millis(250);

    pub fn new(window: Duration) -> Self {
        Self { window, last: None }
    }

    /// `true` to handle this Format List, `false` when it repeats the
    /// previous one within the window. `formats` is `(format id, name)`.
    pub fn admit<'a>(
        &mut self,
        formats: impl IntoIterator<Item = (u32, Option<&'a str>)>,
        now: Instant,
    ) -> bool {
        let list: FormatList = formats
            .into_iter()
            .map(|(id, name)| (id, name.map(str::to_owned)))
            .collect();
        let echo = matches!(&self.last, Some((prev, at))
            if *prev == list && now.saturating_duration_since(*at) <= self.window);
        self.last = Some((list, now));
        !echo
    }

    /// Forget the previous list (session end).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

impl Default for FormatListDedup {
    fn default() -> Self {
        Self::new(Self::DEFAULT_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILES: [(u32, Option<&str>); 3] = [
        (49386, Some("FileGroupDescriptorW")),
        (49385, Some("FileContents")),
        (49384, Some("Preferred DropEffect")),
    ];
    const TEXT: [(u32, Option<&str>); 2] = [(13, None), (1, None)];

    #[test]
    fn format_list_echo_within_window_is_dropped() {
        let mut d = FormatListDedup::default();
        let t0 = Instant::now();
        assert!(d.admit(FILES, t0));
        assert!(!d.admit(FILES, t0 + Duration::from_micros(50)));
        // A burst keeps collapsing: the window restarts on every list.
        assert!(!d.admit(FILES, t0 + Duration::from_millis(200)));
        assert!(!d.admit(FILES, t0 + Duration::from_millis(400)));
    }

    #[test]
    fn format_list_repeat_after_window_or_changed_is_admitted() {
        let mut d = FormatListDedup::default();
        let t0 = Instant::now();
        assert!(d.admit(FILES, t0));
        assert!(d.admit(FILES, t0 + Duration::from_secs(1)), "a later copy of the same kind");
        assert!(d.admit(TEXT, t0 + Duration::from_millis(1001)), "different formats");
        assert!(
            d.admit(FILES, t0 + Duration::from_millis(1002)),
            "differs from the previous list, not from an older one"
        );
        let mut renamed = FILES;
        renamed[2].1 = Some("Shell IDList Array");
        assert!(d.admit(renamed, t0 + Duration::from_millis(1003)), "same ids, other names");
    }

    #[test]
    fn format_list_reset_forgets_the_previous_list() {
        let mut d = FormatListDedup::default();
        let t0 = Instant::now();
        assert!(d.admit(FILES, t0));
        d.reset();
        assert!(d.admit(FILES, t0), "a new session's first list is never an echo");
    }

    fn entry<'a>(dir: Option<&'a str>, name: &'a str) -> RemoteEntry<'a> {
        RemoteEntry {
            relative_dir: dir,
            name,
            is_directory: None,
        }
    }

    #[test]
    fn explorer_directory_copy_keeps_its_tree() {
        let plan = plan_staged_tree(&[
            entry(None, "big.bin"),
            entry(None, "dir one"),
            entry(Some("dir one"), "sub"),
            entry(Some("dir one\\sub"), "nested.txt"),
            entry(Some("dir one"), "zero.dat"),
        ]);
        let got: Vec<_> = plan
            .iter()
            .map(|e| (e.path.to_str().unwrap(), e.is_directory, e.top_level))
            .collect();
        assert_eq!(
            got,
            vec![
                ("big.bin", false, true),
                ("dir one", true, true),
                ("dir one/sub", true, false),
                ("dir one/sub/nested.txt", false, false),
                ("dir one/zero.dat", false, false),
            ]
        );
    }

    #[test]
    fn full_path_names_and_explicit_attributes() {
        let plan = plan_staged_tree(&[
            RemoteEntry {
                relative_dir: None,
                name: "empty dir",
                is_directory: Some(true),
            },
            entry(None, "a\\b.txt"),
        ]);
        assert!(plan[0].is_directory, "explicit attribute wins over inference");
        assert_eq!(plan[1].path, PathBuf::from("a/b.txt"));
        assert!(!plan[1].top_level);
    }

    #[test]
    fn hostile_names_stay_inside_the_staging_dir() {
        let plan = plan_staged_tree(&[
            entry(Some("..\\..\\etc"), "passwd"),
            entry(None, "/abs"),
            entry(None, "."),
            entry(None, ""),
            entry(Some("x"), "a\u{7}b"),
        ]);
        let paths: Vec<_> = plan.iter().map(|e| e.path.to_str().unwrap()).collect();
        assert_eq!(paths, vec!["_/_/etc/passwd", "_abs", "_", "_", "x/ab"]);
        assert!(plan.iter().all(|e| e.path.is_relative()));
    }

    #[test]
    fn long_component_is_cut_on_a_char_boundary() {
        let long = "é".repeat(200); // 400 bytes
        let s = sanitize_component(&long);
        assert!(s.len() <= MAX_COMPONENT_BYTES);
        assert_eq!(s.len(), 254);
    }

    #[test]
    fn take_pending_file_paste_skips_text_and_prefetch() {
        let mut pending: VecDeque<(u32, String, ())> = VecDeque::from(vec![
            (u32::MAX, "text/uri-list".to_string(), ()),
            (1, "text/plain;charset=utf-8".to_string(), ()),
            (2, "x-special/gnome-copied-files".to_string(), ()),
            (3, "text/uri-list".to_string(), ()),
        ]);
        let got = take_pending_file_paste(&mut pending, u32::MAX);
        assert_eq!(got.map(|(s, m, _)| (s, m)), Some((2, "x-special/gnome-copied-files".to_string())));
        assert_eq!(pending.len(), 3, "only the answered request is removed");
        assert_eq!(pending[1].0, 1, "the text request keeps its place");
    }

    fn temp_tree() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "hyperv-rdp-extras-clip-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("gdir x/sub")).unwrap();
        std::fs::create_dir_all(base.join("gdir x/empty")).unwrap();
        std::fs::write(base.join("g.bin"), b"0123456789").unwrap();
        std::fs::write(base.join("gdir x/b.txt"), b"bb").unwrap();
        std::fs::write(base.join("gdir x/sub/inner.txt"), b"inner").unwrap();
        base
    }

    #[test]
    fn expand_outgoing_walks_directories_parents_first() {
        let base = temp_tree();
        let (entries, skipped) =
            expand_outgoing(&[base.join("g.bin"), base.join("gdir x")], 1000);
        let got: Vec<_> = entries
            .iter()
            .map(|e| (e.relative_dir.as_deref(), e.name.as_str(), e.is_directory, e.size))
            .collect();
        assert_eq!(
            got,
            vec![
                (None, "g.bin", false, 10),
                (None, "gdir x", true, 0),
                (Some("gdir x"), "b.txt", false, 2),
                (Some("gdir x"), "empty", true, 0),
                (Some("gdir x"), "sub", true, 0),
                (Some("gdir x\\sub"), "inner.txt", false, 5),
            ]
        );
        assert_eq!(skipped, 0);
        assert_eq!(entries[5].path, base.join("gdir x/sub/inner.txt"));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn expand_outgoing_caps_entries_and_skips_missing_roots() {
        let base = temp_tree();
        let (entries, skipped) =
            expand_outgoing(&[base.join("missing"), base.join("gdir x")], 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(skipped, 1 + 2, "missing root + two entries past the cap (sub/ is never entered)");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn expand_outgoing_skips_over_long_wire_names() {
        let base = temp_tree();
        let deep = base.join("gdir x").join("n".repeat(250));
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f.txt"), b"f").unwrap();
        let (entries, skipped) = expand_outgoing(&[base.join("gdir x")], 1000);
        assert!(entries.iter().any(|e| e.name.len() == 250), "250 + 7 units fits");
        assert!(entries.iter().all(|e| e.name != "f.txt"), "257 + 6 units does not");
        assert_eq!(skipped, 1);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn take_pending_file_paste_none_without_a_file_request() {
        let mut pending: VecDeque<(u32, String, ())> =
            VecDeque::from(vec![(1, "text/plain".to_string(), ())]);
        assert!(take_pending_file_paste(&mut pending, u32::MAX).is_none());
        assert_eq!(pending.len(), 1);
    }
}
