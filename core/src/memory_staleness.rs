//! Memory staleness invalidation when referenced code changes (spec §16).
//!
//! When files listed in a memory's `ref_files` / `files:` frontmatter change,
//! mark the memory `needs_verification` and lower its retrieval weight via
//! [`MemoryStatus::rank_multiplier`]. Unrelated path changes do not invalidate.
//! Fail-open: I/O errors skip individual memories without aborting the turn.

#![allow(dead_code)]

use std::path::Path;

use crate::memory::{self, MemoryEntry, MemoryStatus, Scope};

/// Given `changed` relative paths, mark overlapping memories `needs_verification`.
/// Returns the names of memories that were marked (deduped, sorted).
pub fn invalidate_for_paths(workspace: &Path, changed: &[String]) -> Vec<String> {
    if changed.is_empty() {
        return Vec::new();
    }
    let changed_norm: Vec<String> = changed.iter().map(|p| normalize_path(p)).collect();
    let memories = memory::scan_all_memories(workspace);
    let mut marked = Vec::new();

    for entry in memories {
        if entry.ref_files.is_empty() {
            continue;
        }
        if !entry.status.is_positive_guidance() {
            continue;
        }
        if !refs_overlap(&entry.ref_files, &changed_norm) {
            continue;
        }
        if entry.status == MemoryStatus::NeedsVerification {
            // Already marked — still report name so callers see it.
            if !marked.iter().any(|n| n == &entry.name) {
                marked.push(entry.name.clone());
            }
            continue;
        }
        match rewrite_status(workspace, &entry, MemoryStatus::NeedsVerification, None) {
            Ok(()) => marked.push(entry.name.clone()),
            Err(_) => {
                // Fail-open: skip corrupt/unwritable entry.
            }
        }
    }

    marked.sort();
    marked.dedup();
    if !marked.is_empty() {
        memory::invalidate_scan_cache();
    }
    marked
}

/// Restore a memory to `verified` and refresh `last_verified_at`.
/// Searches workspace then global scope. Returns Ok(name) on success.
pub fn verify_memory(workspace: &Path, name: &str) -> Result<String, String> {
    let entry = memory::get_memory(workspace, name)
        .or_else(|_| memory::get_memory_scoped(workspace, Scope::Global, name))?;
    let now = now_secs();
    rewrite_status(workspace, &entry, MemoryStatus::Verified, Some(now))?;
    memory::invalidate_scan_cache();
    Ok(entry.name)
}

fn refs_overlap(ref_files: &[String], changed_norm: &[String]) -> bool {
    for r in ref_files {
        let rn = normalize_path(r);
        for c in changed_norm {
            if paths_match(&rn, c) {
                return true;
            }
        }
    }
    false
}

fn paths_match(a: &str, b: &str) -> bool {
    a == b || a.ends_with(b) || b.ends_with(a)
}

fn normalize_path(p: &str) -> String {
    p.trim().trim_start_matches("./").replace('\\', "/")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Rewrite lifecycle metadata against the latest on-disk snapshot. This avoids
/// a stale scan undoing an append or another metadata update.
fn rewrite_status(
    workspace: &Path,
    entry: &MemoryEntry,
    status: MemoryStatus,
    last_verified_at: Option<u64>,
) -> Result<(), String> {
    memory::update_memory_scoped(workspace, entry.scope, &entry.name, |updated| {
        updated.status = status;
        updated.schema_version = updated.schema_version.max(2);
        if let Some(ts) = last_verified_at {
            updated.last_verified_at = Some(ts);
        }
    })
    .map(|_| ())
}

/// Local mirror of memory::write_memory_file (private there).
fn write_memory_file_public(path: &Path, e: &MemoryEntry) -> Result<(), String> {
    let pin_line = if e.pinned { "pin: true\n" } else { "" };
    let importance_line = if e.importance != memory::Importance::Normal {
        format!("importance: {}\n", e.importance.as_str())
    } else {
        String::new()
    };
    let dep_line = if e.deprecated {
        "deprecated: true\n".to_string()
    } else {
        String::new()
    };
    let sup_line = match &e.superseded_by {
        Some(s) if !s.trim().is_empty() => format!("superseded_by: {s}\n"),
        _ => String::new(),
    };
    let mut v2 = String::new();
    v2.push_str(&format!("schema_version: {}\n", e.schema_version.max(2)));
    v2.push_str(&format!("status: {}\n", e.status.as_str()));
    if (e.confidence - 1.0).abs() > f32::EPSILON {
        v2.push_str(&format!("confidence: {:.2}\n", e.confidence));
    }
    if e.support_count > 0 {
        v2.push_str(&format!("support_count: {}\n", e.support_count));
    }
    if e.contradiction_count > 0 {
        v2.push_str(&format!("contradiction_count: {}\n", e.contradiction_count));
    }
    if let Some(t) = e.last_verified_at {
        v2.push_str(&format!("last_verified_at: {t}\n"));
    }
    if let Some(ref c) = e.last_verified_commit {
        v2.push_str(&format!("last_verified_commit: {c}\n"));
    }
    if !e.ref_files.is_empty() {
        v2.push_str("files: ");
        v2.push_str(&e.ref_files.join(", "));
        v2.push('\n');
    }
    if !e.ref_symbols.is_empty() {
        v2.push_str("symbols: ");
        v2.push_str(&e.ref_symbols.join(", "));
        v2.push('\n');
    }
    let body = format!(
        "---\nname: {}\ndescription: {}\ntype: {}\n{pin_line}{importance_line}{dep_line}{sup_line}{v2}---\n{}",
        e.name, e.description, e.mem_type, e.content
    );
    crate::fsutil::atomic_write_str(path, &body).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{override_memory_root, scan_all_memories, MemoryStatus};
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn with_temp_store<R>(f: impl FnOnce(&Path) -> R) -> R {
        let _serial = memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let n = N.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!(
            "mem-stale-{}-{}-{}",
            std::process::id(),
            n,
            now_secs()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let mem_root = base.join("memory");
        let ws = base.join("ws");
        std::fs::create_dir_all(&mem_root).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let _guard = override_memory_root(mem_root);
        f(&ws)
    }

    fn write_mem(ws: &Path, name: &str, files: &str, status: &str) {
        memory::save_memory(ws, name, "body about architecture", "architecture", "desc").unwrap();
        memory::invalidate_scan_cache();
        let entries = scan_all_memories(ws);
        let e = entries.iter().find(|e| e.name == name).unwrap_or_else(|| {
            panic!(
                "saved memory '{name}' not found; got {:?}",
                entries.iter().map(|e| &e.name).collect::<Vec<_>>()
            )
        });
        let body = format!(
            "---\nname: {name}\ndescription: desc\ntype: architecture\nschema_version: 2\nstatus: {status}\nfiles: {files}\n---\nbody about architecture\n"
        );
        std::fs::write(&e.path, body).unwrap();
        memory::invalidate_scan_cache();
    }

    #[test]
    fn overlapping_path_marks_needs_verification() {
        with_temp_store(|ws| {
            write_mem(
                ws,
                "provider-arch",
                "core/src/provider.rs, core/src/plugins.rs",
                "verified",
            );
            let marked = invalidate_for_paths(ws, &["core/src/provider.rs".into()]);
            assert_eq!(marked, vec!["provider-arch".to_string()]);
            let entries = scan_all_memories(ws);
            let e = entries.iter().find(|e| e.name == "provider-arch").unwrap();
            assert_eq!(e.status, MemoryStatus::NeedsVerification);
        });
    }

    #[test]
    fn unrelated_path_does_not_invalidate() {
        with_temp_store(|ws| {
            write_mem(ws, "provider-arch", "core/src/provider.rs", "verified");
            let marked = invalidate_for_paths(ws, &["core/src/memory.rs".into()]);
            assert!(marked.is_empty());
            let entries = scan_all_memories(ws);
            let e = entries.iter().find(|e| e.name == "provider-arch").unwrap();
            assert_eq!(e.status, MemoryStatus::Verified);
        });
    }

    #[test]
    fn verify_memory_restores_verified() {
        with_temp_store(|ws| {
            write_mem(
                ws,
                "provider-arch",
                "core/src/provider.rs",
                "needs_verification",
            );
            let name = verify_memory(ws, "provider-arch").unwrap();
            assert_eq!(name, "provider-arch");
            let entries = scan_all_memories(ws);
            let e = entries.iter().find(|e| e.name == "provider-arch").unwrap();
            assert_eq!(e.status, MemoryStatus::Verified);
            assert!(e.last_verified_at.is_some());
        });
    }
}
