//! The shell's heap (helixdesktop/desktop#120): glibc malloc's own counts,
//! logged now and then when `HELIX_SHELL_HEAP_LOG` is a number of
//! seconds, so a long run shows whether memory grows because it is in
//! use (a leak, a cache) or because freed memory is kept (fragmentation,
//! many arenas).
//!
//! What a long headless run showed: the heap in use stays a few MiB while
//! malloc keeps tens of MiB it freed (startup pictures, surfaces drawn
//! and dropped), spread over arenas of many threads (the D-Bus runtime's
//! workers, picture and icon workers). So the shell caps malloc's arenas
//! ([`limit_arenas`]) and gives freed memory back now and then
//! ([`Trim`]).

/// glibc malloc's counts (all arenas), in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Memory malloc took from the system for its arenas (not mmapped
    /// chunks).
    pub arena: usize,
    /// In use by the program.
    pub in_use: usize,
    /// Free but kept in the arenas.
    pub free: usize,
    /// Large chunks mmapped on their own.
    pub mmapped: usize,
}

/// The counts now.
pub fn stats() -> Stats {
    // SAFETY: mallinfo2 only reads malloc's state.
    let info = unsafe { libc::mallinfo2() };
    Stats { arena: info.arena, in_use: info.uordblks, free: info.fordblks, mmapped: info.hblkhd }
}

/// With `HELIX_SHELL_HEAP_LOG=<seconds>`: log [`stats`] that often (the
/// soak test reads them).
pub fn log_from_env() {
    let Some(every) = std::env::var("HELIX_SHELL_HEAP_LOG").ok().and_then(|s| s.parse::<u64>().ok()).filter(|s| *s > 0) else { return };
    let _ = std::thread::Builder::new().name("heap-log".into()).spawn(move || loop {
        let s = stats();
        tracing::info!(arena = s.arena, in_use = s.in_use, free = s.free, mmapped = s.mmapped, "shell heap");
        std::thread::sleep(std::time::Duration::from_secs(every));
    });
}

/// Arenas malloc may make (glibc's default is 8 per core: 256 on a
/// 32-thread machine, each keeping what its threads freed).
const ARENAS: libc::c_int = 4;

/// `HELIX_SHELL_HEAP_DEFAULTS` set: malloc as glibc sets it up, no
/// trimming (to compare).
fn defaults() -> bool {
    std::env::var_os("HELIX_SHELL_HEAP_DEFAULTS").is_some()
}

/// Cap malloc's arenas; before any other thread starts.
pub fn limit_arenas() {
    if defaults() {
        return;
    }
    // SAFETY: mallopt only sets malloc's tunables.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, ARENAS);
    }
}

/// Free memory malloc keeps beyond this is given back (at most every
/// [`TRIM_EVERY`]).
const TRIM_ABOVE: usize = 16 << 20;
const TRIM_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Gives memory malloc keeps back to the system now and then.
#[derive(Debug, Default)]
pub struct Trim {
    last: Option<std::time::Instant>,
}

impl Trim {
    /// Called often (each second): trims when malloc keeps more than
    /// [`TRIM_ABOVE`] free and the last trim is [`TRIM_EVERY`] ago.
    pub fn tick(&mut self) {
        let now = std::time::Instant::now();
        if self.last.is_some_and(|t| now.duration_since(t) < TRIM_EVERY) || defaults() {
            return;
        }
        self.last = Some(now);
        let before = stats();
        if before.free < TRIM_ABOVE {
            return;
        }
        // SAFETY: malloc_trim only returns free memory to the system.
        unsafe {
            libc::malloc_trim(0);
        }
        tracing::debug!(free = before.free, took_ms = now.elapsed().as_millis() as u64, "heap trimmed");
    }
}
