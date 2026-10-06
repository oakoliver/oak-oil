//! Parallel directory walk with a shared work stack.

use std::fs::{self, DirEntry};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

/// Walks from `roots` on `threads` workers. `visit` gets each directory and its
/// entries, and returns the subdirectories to descend into.
pub fn parallel_walk<F>(roots: Vec<PathBuf>, threads: usize, visit: F)
where
    F: Fn(&Path, Vec<DirEntry>) -> Vec<PathBuf> + Sync,
{
    // (pending dirs, dirs being visited right now)
    let state = Mutex::new((roots, 0usize));
    let cv = Condvar::new();

    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                loop {
                    let dir = {
                        let mut st = state.lock().unwrap();
                        loop {
                            if let Some(d) = st.0.pop() {
                                st.1 += 1;
                                break Some(d);
                            }
                            if st.1 == 0 {
                                break None;
                            }
                            st = cv.wait(st).unwrap();
                        }
                    };
                    let Some(dir) = dir else {
                        cv.notify_all();
                        return;
                    };

                    let entries = match fs::read_dir(&dir) {
                        Ok(rd) => rd.filter_map(Result::ok).collect(),
                        Err(_) => Vec::new(),
                    };
                    let children = visit(&dir, entries);

                    let mut st = state.lock().unwrap();
                    st.0.extend(children);
                    st.1 -= 1;
                    cv.notify_all();
                }
            });
        }
    });
}

/// Subdirectories of `entries`, never following symlinks.
pub fn subdirs(entries: &[DirEntry]) -> impl Iterator<Item = &DirEntry> {
    entries
        .iter()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
}
