// Phase-aware progress tracking for package operations.
//
// Package managers never report one percentage for a whole operation: every
// phase counts its own work, and the raw stream contains percentages that
// belong to unrelated work.  `dnf5 install`, for example, prints one `100%`
// line per repository *while loading metadata* — parsed naively (what
// `operations::run_process` used to do) the orb pins at full before a single
// package has been downloaded and can never move again.
//
// A `Tracker` is fed every output line of one operation and maps it to a
// single 0..1 fraction of that operation:
//
// * **flatpak** prints `Installing 1/3…` per operation and repeats it on each
//   `NN%` progress line → `((op - 1) + pct) / ops`, exact for install,
//   update and uninstall alike (captured from flatpak 1.18 piped output).
// * **dnf4/dnf5** print one finished bar per unit of work — `[1/4] pkg… 100%`
//   while downloading, `[3/6] Installing pkg… 100%` while transacting (both
//   captured from dnf5 5.4).  Each phase gets half the arc; `remove` has no
//   download phase, so the transaction owns the whole arc.  Metadata lines
//   carry no counter and are deliberately ignored.
// * **apt / pacman / zypper** are unit-counted against their printed
//   summaries (best effort — derived from those tools' output formats, they
//   are not installed on the dev machine).
// * **snap** has no interpretable signal when piped → whatever `NN%` it does
//   print is passed through unchanged (the legacy behaviour).
// * chained "update all" commands (`sh -c '… && …'`) are sliced per tool by
//   `__spotty_part_k_m_tool__` markers injected in `search::cmd`.
// * anything else falls back to the generic `parse_percent` reader.
//
// Fractions below `MIN_FRACTION` are reported as unknown so the orb keeps
// spinning until there is a visible arc.

use crate::operations::parse_percent;

/// Marker echoed by chained update commands before each part, so the tracker
/// knows which tool is running and how far into the whole run we are.
const MARKER_PREFIX: &str = "__spotty_part_";
const MARKER_SUFFIX: &str = "__";

/// Smallest fraction worth drawing (≈1.8° of arc); below this the orb keeps
/// spinning instead of showing a dot.
const MIN_FRACTION: f64 = 0.005;

/// True when `line` is one of the internal `__spotty_part_…__` markers —
/// `run_process` keeps these out of the visible status text.
pub(crate) fn is_part_marker(line: &str) -> bool {
    let t = line.trim();
    t.starts_with(MARKER_PREFIX) && t.ends_with(MARKER_SUFFIX) && t.len() > MARKER_PREFIX.len()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Flatpak,
    Dnf,
    Apt,
    Pacman,
    Zypper,
    Snap,
    Sh,
    Unknown,
}

/// Which tool an operation's argv actually runs.  Wrappers the command
/// builders prepend (`flatpak-spawn --host`, `pkexec`, …) are skipped so the
/// real program decides how its output is parsed.
fn tool_from_argv(argv: &[String]) -> Tool {
    let mut i = 0;
    while let Some(tok) = argv.get(i) {
        let base = tok.rsplit('/').next().unwrap_or(tok);
        match base {
            "flatpak-spawn" => {
                i += 1;
                if argv.get(i).is_some_and(|s| s.starts_with('-')) {
                    i += 1;
                }
            }
            "pkexec" | "sudo" | "nice" => i += 1,
            "env" => {
                i += 1;
                while argv.get(i).is_some_and(|s| s.contains('=')) {
                    i += 1;
                }
            }
            "sh" | "bash" | "dash" | "zsh" => return Tool::Sh,
            "flatpak" => return Tool::Flatpak,
            "dnf" | "dnf4" | "dnf5" => return Tool::Dnf,
            "apt" | "apt-get" | "aptitude" => return Tool::Apt,
            "pacman" => return Tool::Pacman,
            "zypper" => return Tool::Zypper,
            "snap" => return Tool::Snap,
            _ => return Tool::Unknown,
        }
    }
    Tool::Unknown
}

/// Tool name as written into a `__spotty_part_k_m_tool__` marker.
fn tool_by_name(name: &str) -> Tool {
    match name {
        "flatpak" => Tool::Flatpak,
        "dnf" => Tool::Dnf,
        "apt" => Tool::Apt,
        "pacman" => Tool::Pacman,
        "zypper" => Tool::Zypper,
        "snap" => Tool::Snap,
        _ => Tool::Unknown,
    }
}

/// Download/transaction halves of a two-phase operation (dnf, apt, pacman,
/// zypper).  The download phase owns the first half of the arc and the
/// transaction the rest; a command with no download phase (a `remove`) maps
/// the transaction onto the whole arc.
#[derive(Default)]
struct Phase {
    dl: Option<f64>,
    tx: Option<f64>,
    started: bool,
}

impl Phase {
    fn set_dl(&mut self, v: f64) {
        // Once the transaction has begun the download half is frozen — a late
        // line from the other pipe must not yank the fraction backwards.
        if !self.started {
            self.dl = Some(self.dl.unwrap_or(0.0).max(v));
        }
    }

    fn set_tx(&mut self, v: f64) {
        self.started = true;
        self.tx = Some(self.tx.unwrap_or(0.0).max(v));
    }

    fn raw(&self) -> f64 {
        let base = self.dl.map_or(0.0, |d| 0.5 * d);
        match self.tx {
            Some(t) => base + (1.0 - base) * t,
            None => base,
        }
    }
}

/// Maps one operation's output lines to a single 0..1 progress fraction.
pub(crate) struct Tracker {
    tool: Tool,
    // flatpak: current operation / total operations / its last percentage
    fp_op: usize,
    fp_ops: usize,
    fp_pct: f64,
    // two-phase tools (dnf write dl/tx directly, apt/zypper count events)
    phase: Phase,
    exp_dl: f64,
    exp_tx: f64,
    ev_dl: f64,
    ev_tx: f64,
    // chained `sh -c` runs: which part is running, and that part's tracker
    part_k: usize,
    part_m: usize,
    sub: Option<Box<Tracker>>,
    // legacy per-line percentage for tools we can't interpret
    generic: Option<f64>,
}

impl Tracker {
    /// Build a tracker for an operation running `argv` (`argv[0]` the program).
    pub(crate) fn new(argv: &[String]) -> Self {
        Self::bare(tool_from_argv(argv))
    }

    fn bare(tool: Tool) -> Self {
        Tracker {
            tool,
            fp_op: 0,
            fp_ops: 0,
            fp_pct: 0.0,
            phase: Phase::default(),
            exp_dl: 0.0,
            exp_tx: 0.0,
            ev_dl: 0.0,
            ev_tx: 0.0,
            part_k: 0,
            part_m: 0,
            sub: None,
            generic: None,
        }
    }

    /// Feed one output line (already split on `\n`/`\r` by `run_process`).
    pub(crate) fn feed(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match self.tool {
            Tool::Flatpak => self.feed_flatpak(line),
            Tool::Dnf => self.feed_dnf(line),
            Tool::Apt => self.feed_apt(line),
            Tool::Pacman => self.feed_pacman(line),
            Tool::Zypper => self.feed_zypper(line),
            Tool::Sh => self.feed_sh(line),
            Tool::Snap | Tool::Unknown => {
                if let Some(p) = parse_percent(line) {
                    self.generic = Some(p);
                }
            }
        }
    }

    /// Current fraction of the whole operation, or `None` while there is no
    /// trustworthy signal (the orb then spins instead of lying).
    pub(crate) fn fraction(&self) -> Option<f64> {
        let raw = self.raw().clamp(0.0, 1.0);
        if raw > MIN_FRACTION {
            Some(raw)
        } else {
            None
        }
    }

    fn raw(&self) -> f64 {
        match self.tool {
            Tool::Flatpak => {
                if self.fp_ops == 0 {
                    return 0.0;
                }
                let done = self.fp_op.saturating_sub(1) as f64;
                (done + self.fp_pct) / self.fp_ops as f64
            }
            Tool::Dnf | Tool::Apt | Tool::Pacman | Tool::Zypper => self.phase.raw(),
            Tool::Snap | Tool::Unknown => self.generic.unwrap_or(0.0),
            Tool::Sh => {
                if self.part_m == 0 {
                    // No marker yet: behave exactly like the old generic reader.
                    return self.generic.unwrap_or(0.0);
                }
                let inner = self
                    .sub
                    .as_ref()
                    .map(|s| s.raw())
                    .unwrap_or_else(|| self.generic.unwrap_or(0.0));
                let base = self.part_k.max(1) as f64 - 1.0;
                (base + inner) / self.part_m as f64
            }
        }
    }

    // ── flatpak ──────────────────────────────────────────────────────────────

    fn feed_flatpak(&mut self, line: &str) {
        let Some((op, ops, pct)) = flatpak_line(line) else {
            return;
        };
        if ops > 0 {
            if op != self.fp_op {
                // A new operation starts at its own 0%.
                self.fp_pct = 0.0;
            }
            self.fp_op = op;
            self.fp_ops = ops;
        }
        if let Some(p) = pct {
            self.fp_pct = p;
        }
    }

    // ── dnf4 / dnf5 ──────────────────────────────────────────────────────────

    fn feed_dnf(&mut self, line: &str) {
        // dnf5: "[3/6] Installing recode-0:3.7.15-3.fc44 100% | …" (transaction)
        //       "[1/4] sl-0:5.02-25.fc44.x86_64       100% | …" (download)
        if let Some((n, m, rest)) = leading_counter(line) {
            let v = (n / m).clamp(0.0, 1.0);
            if is_tx_desc(rest) {
                self.phase.set_tx(v);
            } else {
                self.phase.set_dl(v);
            }
            return;
        }
        // dnf4 puts the counter at the end: "  Upgrading : foo-1.0-1.x86_64  1/3"
        if is_tx_desc(line) {
            if let Some((n, m)) = trailing_counter(line) {
                self.phase.set_tx((n / m).clamp(0.0, 1.0));
            }
        }
    }

    // ── apt (best effort) ────────────────────────────────────────────────────

    fn feed_apt(&mut self, line: &str) {
        let t = line.trim_start();
        // "3 upgraded, 0 newly installed, 0 to remove and 1 not upgraded."
        if t.contains("upgraded,") && t.contains("newly installed") {
            if let (Some(u), Some(n)) = (
                digits_before(t, "upgraded"),
                digits_before(t, "newly installed"),
            ) {
                let r = digits_before(t, "to remove").unwrap_or(0.0);
                self.exp_dl = u + n; // packages apt fetches
                self.exp_tx = (u + n) * 2.0 + r; // unpack + setup, plus removals
            }
            return;
        }
        if t.starts_with("Get:") {
            self.ev_dl += 1.0;
            if self.exp_dl > 0.0 {
                self.phase.set_dl((self.ev_dl / self.exp_dl).clamp(0.0, 1.0));
            }
            return;
        }
        let is_event = t.starts_with("Unpacking ")
            || t.starts_with("Setting up ")
            || t.starts_with("Removing ")
            || t.starts_with("Purging ");
        if is_event {
            self.ev_tx += 1.0;
            if self.exp_tx > 0.0 {
                self.phase.set_tx((self.ev_tx / self.exp_tx).clamp(0.0, 1.0));
            }
        }
    }

    // ── pacman (best effort) ─────────────────────────────────────────────────

    fn feed_pacman(&mut self, line: &str) {
        // "(1/5) installing foo" — each alpm phase restarts its counter, so
        // only the commit phase moves the arc; hook/keyring steps are skipped.
        let Some((n, m, rest)) = leading_counter(line) else {
            return;
        };
        let what = rest.trim_start();
        if what.starts_with("installing")
            || what.starts_with("upgrading")
            || what.starts_with("downgrading")
            || what.starts_with("reinstalling")
            || what.starts_with("removing")
        {
            self.phase.set_tx((n / m).clamp(0.0, 1.0));
        }
    }

    // ── zypper (best effort) ─────────────────────────────────────────────────

    fn feed_zypper(&mut self, line: &str) {
        let t = line.trim_start();
        // "The following 2 packages are going to be installed:" (+ upgraded/removed)
        if t.contains("going to be installed")
            || t.contains("going to be upgraded")
            || t.contains("going to be removed")
        {
            if let Some(c) = digits_before(t, "going to be") {
                self.exp_tx += c;
                if !t.contains("going to be removed") {
                    self.exp_dl += c;
                }
            }
            return;
        }
        if !t.contains("[done") {
            return;
        }
        if t.starts_with("Installing:") || t.starts_with("Upgrading:") || t.starts_with("Removing:")
        {
            self.ev_tx += 1.0;
            if self.exp_tx > 0.0 {
                self.phase.set_tx((self.ev_tx / self.exp_tx).clamp(0.0, 1.0));
            }
        } else if t.starts_with("Downloading:") || t.starts_with("Retrieving:") {
            self.ev_dl += 1.0;
            if self.exp_dl > 0.0 {
                self.phase.set_dl((self.ev_dl / self.exp_dl).clamp(0.0, 1.0));
            }
        }
    }

    // ── chained `sh -c` runs ─────────────────────────────────────────────────

    fn feed_sh(&mut self, line: &str) {
        if let Some((k, m, tool)) = part_marker(line) {
            self.part_k = k;
            self.part_m = m;
            self.sub = Some(Box::new(Tracker::bare(tool)));
            return;
        }
        if let Some(sub) = self.sub.as_mut() {
            sub.feed(line);
            return;
        }
        if let Some(p) = parse_percent(line) {
            self.generic = Some(p);
        }
    }
}

/// Parse `__spotty_part_k_m_tool__` → `(k, m, tool)`.
fn part_marker(line: &str) -> Option<(usize, usize, Tool)> {
    let body = line
        .trim()
        .strip_prefix(MARKER_PREFIX)?
        .strip_suffix(MARKER_SUFFIX)?;
    let mut it = body.split('_');
    let k = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    if k == 0 || m == 0 || k > m {
        return None;
    }
    Some((k, m, tool_by_name(it.next()?)))
}

/// flatpak CLI line → `(operation, total operations, percent of that op)`.
///
/// Non-fancy (piped) output prints `Installing 1/3…` per operation and repeats
/// that prefix on every progress line: `Installing 1/3… ███░░  45%  1.2 MB/s`.
/// Single-operation runs print the bare `Installing…` form.
fn flatpak_line(line: &str) -> Option<(usize, usize, Option<f64>)> {
    const WORDS: [&str; 3] = ["Installing", "Updating", "Uninstalling"];
    let word = WORDS.iter().find(|w| line.starts_with(**w))?;
    let rest = &line[word.len()..];
    let (op, ops, tail): (usize, usize, &str) = if let Some(t) = rest.strip_prefix('\u{2026}') {
        (1, 1, t)
    } else if let Some(r) = rest.strip_prefix(' ') {
        let (counter, tail) = r.split_once('\u{2026}')?;
        let (a, b) = counter.split_once('/')?;
        (a.trim().parse().ok()?, b.trim().parse().ok()?, tail)
    } else {
        return None;
    };
    Some((op, ops, pct_of(tail)))
}

/// The `NN` in `NN%`, without `parse_percent`'s counter fallback.
fn pct_of(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'%' {
            let mut j = i;
            while j > 0 && bytes[j - 1].is_ascii_digit() {
                j -= 1;
            }
            if j < i {
                if let Ok(n) = s[j..i].parse::<f64>() {
                    return Some((n / 100.0).clamp(0.0, 1.0));
                }
            }
        }
    }
    None
}

/// Descriptions dnf puts on its *transaction* bars.  Download bars are bare
/// package NEVRAs (`sl-0:5.02-25.fc44.x86_64`, `Total`), so a leading action
/// verb is what separates the two phases.
const TX_VERBS: [&str; 8] = [
    "Installing",
    "Upgrading",
    "Downgrading",
    "Reinstalling",
    "Removing",
    "Cleanup",
    "Prepare transaction",
    "Verify package files",
];

fn is_tx_desc(s: &str) -> bool {
    let s = s.trim_start();
    TX_VERBS.iter().any(|v| {
        s.strip_prefix(v)
            .is_some_and(|r| r.is_empty() || r.starts_with(' '))
    })
}

/// `[1/4] rest…` or `(2/5): rest…` at the start of a line.
fn leading_counter(s: &str) -> Option<(f64, f64, &str)> {
    let t = s.trim_start();
    let close = match t.as_bytes().first()? {
        b'[' => b']',
        b'(' => b')',
        _ => return None,
    };
    let rel = t.as_bytes()[1..].iter().position(|&c| c == close)?;
    let inner = &t[1..1 + rel];
    let (a, b) = inner.split_once('/')?;
    let (a, b) = (a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?);
    if a < 0.0 || b <= 0.0 {
        return None;
    }
    Some((a, b, t[1 + rel + 1..].trim_start()))
}

/// `n/m` as the final whitespace-delimited token (`  1/3`).
fn trailing_counter(s: &str) -> Option<(f64, f64)> {
    let t = s.trim_end();
    let tok = t.rsplit(char::is_whitespace).next()?;
    let (a, b) = tok.split_once('/')?;
    let (a, b) = (a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?);
    if a < 0.0 || b <= 0.0 {
        return None;
    }
    Some((a, b))
}

/// First whole number before `kw` (`"3 upgraded, …"` → 3).
fn digits_before(s: &str, kw: &str) -> Option<f64> {
    let i = s.find(kw)?;
    s[..i]
        .split_whitespace()
        .rev()
        .find_map(|w| w.trim_end_matches(':').parse::<f64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn feed(tracker: &mut Tracker, lines: &[&str]) -> Vec<Option<f64>> {
        lines.iter().map(|l| {
            tracker.feed(l);
            tracker.fraction()
        }).collect()
    }

    // ── flavor detection ─────────────────────────────────────────────────────

    #[test]
    fn detects_tool_behind_wrappers() {
        assert_eq!(
            tool_from_argv(&argv(&["flatpak-spawn", "--host", "flatpak", "install", "x"])),
            Tool::Flatpak
        );
        assert_eq!(
            tool_from_argv(&argv(&["flatpak-spawn", "--host", "pkexec", "dnf", "upgrade", "-y"])),
            Tool::Dnf
        );
        assert_eq!(
            tool_from_argv(&argv(&["pkexec", "apt-get", "remove", "-y", "foo"])),
            Tool::Apt
        );
        assert_eq!(
            tool_from_argv(&argv(&["pkexec", "pacman", "-S", "--noconfirm", "foo"])),
            Tool::Pacman
        );
        assert_eq!(
            tool_from_argv(&argv(&["pkexec", "snap", "install", "foo"])),
            Tool::Snap
        );
        assert_eq!(
            tool_from_argv(&argv(&["sh", "-c", "flatpak update && dnf upgrade"])),
            Tool::Sh
        );
        assert_eq!(tool_from_argv(&argv(&["true"])), Tool::Unknown);
        // A package argument must never be mistaken for the program.
        assert_eq!(
            tool_from_argv(&argv(&["pkexec", "dnf", "install", "-y", "pacman"])),
            Tool::Dnf
        );
    }

    // ── flatpak (captured from flatpak 1.18 piped output) ────────────────────

    #[test]
    fn flatpak_install_walks_every_operation() {
        let mut t = Tracker::new(&argv(&[
            "flatpak-spawn",
            "--host",
            "flatpak",
            "install",
            "--user",
            "--assumeyes",
            "org.gnome.Mines",
        ]));
        let got = feed(
            &mut t,
            &[
                "Looking for matches…",
                "org.gnome.Mines permissions:",
                " 1.\t   \torg.gnome.Mines.Locale\tstable\ti\tflathub\t< 333.9 kB (partial)",
                "Installing 1/2…",
                "Installing 1/2…                        0%  0 bytes/s",
                "Installing 1/2… ████████████████████ 100%",
                "Installing 2/2…",
                "Installing 2/2…                        0%  0 bytes/s",
                "Installing 2/2… ████████████████████ 100%",
                "Installation complete.",
            ],
        );
        assert_eq!(
            got,
            vec![
                None, // "Looking for matches…" is not a progress line
                None,
                None,
                None, // op 1 header: nothing done yet
                None, // 0% of op 1
                Some(0.5), // op 1 complete = 1 of 2
                Some(0.5), // op 2 header
                Some(0.5), // 0% of op 2
                Some(1.0),
                Some(1.0),
            ]
        );
    }

    #[test]
    fn flatpak_single_operation_is_exact() {
        let mut t = Tracker::new(&argv(&["flatpak", "install", "--user", "org.gnome.Foo"]));
        let got = feed(
            &mut t,
            &[
                "Installing…",
                "Installing…                        0%  0 bytes/s",
                "Installing… ████████░░░░░░░░░░░░ 45%  1.2 MB/s",
                "Installing… ████████████████████ 100%",
            ],
        );
        assert_eq!(got[0], None);
        assert_eq!(got[1], None);
        assert_eq!(got[2], Some(0.45));
        assert_eq!(got[3], Some(1.0));
    }

    #[test]
    fn flatpak_uninstall_only_reports_finished_operations() {
        // Piped uninstall prints headers only — no percentages at all.
        let mut t = Tracker::new(&argv(&["flatpak", "uninstall", "--assumeyes", "org.gnome.X"]));
        let got = feed(
            &mut t,
            &[
                "Uninstalling 1/2…",
                "Uninstalling 2/2…",
                "Uninstall complete.",
            ],
        );
        assert_eq!(got, vec![None, Some(0.5), Some(0.5)]);
        // Single-ref uninstall never fakes a fraction; finish() sets 1.0 later.
        let mut t = Tracker::new(&argv(&["flatpak", "uninstall", "--assumeyes", "org.gnome.Y"]));
        let got = feed(&mut t, &["Uninstalling…", "Uninstall complete."]);
        assert_eq!(got, vec![None, None]);
    }

    // ── dnf5 (captured from dnf5 5.4 on Fedora 44) ───────────────────────────

    #[test]
    fn dnf5_metadata_percent_never_moves_the_orb() {
        // Regression: metadata lines used to pin the orb at full before any
        // package had been downloaded.
        let mut t = Tracker::new(&argv(&["pkexec", "dnf", "install", "-y", "cowsay"]));
        let got = feed(
            &mut t,
            &[
                "Updating and loading repositories:",
                " Fedora 44 - x86_64 - Updates           100% |   8.9 KiB/s |  11.8 KiB |  00m01s",
                " Fedora 44 - x86_64                     100% |  28.8 KiB/s |  21.0 KiB |  00m01s",
                "Repositories loaded.",
                "Package      Arch   Version         Repository      Size",
                "Installing:",
                " cowsay      noarch 0:3.8.4-5.fc44  fedora      80.5 KiB",
                "Transaction Summary:",
                " Installing:         1 package",
            ],
        );
        assert!(got.iter().all(|f| f.is_none()), "metadata must stay indeterminate, got {got:?}");
    }

    #[test]
    fn dnf5_install_download_then_transaction() {
        let mut t = Tracker::new(&argv(&["pkexec", "dnf", "install", "-y", "cowsay"]));
        let got = feed(
            &mut t,
            &[
                "Total size of inbound packages is 2 MiB. Need to download 2 MiB.",
                "[1/4] sl-0:5.02-25.fc44.x86_64          100% |  88.3 KiB/s |  16.5 KiB |  00m00s",
                "[2/4] cowsay-0:3.8.4-5.fc44.noarch      100% | 237.0 KiB/s |  54.5 KiB |  00m00s",
                "[4/4] fortune-mod-0:3.26.0-1.fc44.x86_ 100% |   1.6 MiB/s |   1.1 MiB |  00m01s",
                "--------------------------------------------------------------------------------",
                "[4/4] Total                             100% |   2.2 MiB/s |   1.6 MiB |  00m01s",
                "Running transaction",
                "[1/6] Verify package files              100% | 210.0   B/s |   4.0   B |  00m00s",
                "[2/6] Prepare transaction               100% |   9.0   B/s |   4.0   B |  00m00s",
                "[3/6] Installing recode-0:3.7.15-3.fc44 100% |  32.6 MiB/s |   1.2 MiB |  00m00s",
                "Error: call to ldconfig failed.",
                "[6/6] Installing cowsay-0:3.8.4-5.fc44. 100% | 105.7 KiB/s |  89.6 KiB |  00m01s",
                ">>> Running %triggerin scriptlet: glibc-common-0:2.43-8.fc44.x86_64",
            ],
        );
        let expect = [
            None,
            Some(0.5 * 0.25),
            Some(0.5 * 0.5),
            Some(0.5 * 1.0),
            Some(0.5),
            Some(0.5),
            Some(0.5), // "Running transaction" is not progress
            Some(0.5 + 0.5 * (1.0 / 6.0)),
            Some(0.5 + 0.5 * (2.0 / 6.0)),
            Some(0.5 + 0.5 * (3.0 / 6.0)),
            Some(0.5 + 0.5 * (3.0 / 6.0)), // error line: hold
            Some(1.0),
            Some(1.0),
        ];
        for (i, e) in expect.iter().enumerate() {
            match (got[i], e) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 1e-9, "line {i}: {a} != {b}"),
                (a, b) => assert_eq!(a, *b, "line {i}"),
            }
        }
        // Never moves backwards.
        for w in got.windows(2) {
            if let (Some(a), Some(b)) = (w[0], w[1]) {
                assert!(b >= a - 1e-9, "regressed: {a} -> {b}");
            }
        }
    }

    #[test]
    fn dnf5_remove_owns_the_whole_arc() {
        // A removal has no download phase, so the transaction maps 0..1.
        let mut t = Tracker::new(&argv(&["pkexec", "dnf", "remove", "-y", "cowsay"]));
        let got = feed(
            &mut t,
            &[
                " Removing:           3 packages",
                "Running transaction",
                "[1/4] Prepare transaction               100% |   9.0   B/s |   3.0   B |  00m00s",
                "[2/4] Removing cowsay-0:3.8.4-5.fc44.no 100% |   2.4 KiB/s |  63.0   B |  00m00s",
                "[4/4] Removing fortune-mod-0:3.26.0-1.f 100% | 202.0   B/s | 160.0   B |  00m01s",
            ],
        );
        assert_eq!(got, vec![None, None, Some(0.25), Some(0.5), Some(1.0)]);
    }

    #[test]
    fn dnf4_shapes_still_work() {
        let mut t = Tracker::new(&argv(&["pkexec", "dnf", "upgrade", "-y"]));
        let got = feed(
            &mut t,
            &[
                "Fedora 40 - x86_64 - Updates  100% |   1.2 MB/s |   3.3 kB  00:00",
                "(2/5): cowsay-3.8.4-5.fc44.noarch  12 MB/s | 5.2 MB  00:00",
                "(5/5): sl-5.02-25.fc44.x86_64      12 MB/s | 20 kB   00:00",
                "Running transaction",
                "  Upgrading        : cowsay-3.8.4-5.fc44.noarch   1/3",
                "  Cleanup          : cowsay-3.8.3-1.fc44.noarch   3/3",
            ],
        );
        assert_eq!(got[0], None); // bare metadata percent is ignored
        assert_eq!(got[1], Some(0.5 * 0.4));
        assert_eq!(got[2], Some(0.5));
        assert_eq!(got[3], Some(0.5));
        let tx1 = 0.5 + 0.5 * (1.0 / 3.0);
        assert!((got[4].unwrap() - tx1).abs() < 1e-9);
        assert_eq!(got[5], Some(1.0));
    }

    // ── apt / pacman / zypper (output formats, not captured) ─────────────────

    #[test]
    fn apt_counts_download_and_transaction_units() {
        let mut t = Tracker::new(&argv(&["pkexec", "apt-get", "upgrade", "-y"]));
        let got = feed(
            &mut t,
            &[
                "The following packages will be upgraded:",
                "3 upgraded, 0 newly installed, 0 to remove and 1 not upgraded.",
                "Need to get 1234 kB of archives.",
                "Get:1 http://deb.debian.org/debian stable/main amd64 foo amd64 1.0 [400 kB]",
                "Get:2 http://deb.debian.org/debian stable/main amd64 bar amd64 2.0 [500 kB]",
                "Fetched 1234 kB in 1s",
                "Preparing to unpack .../foo_1.0_amd64.deb ...",
                "Unpacking foo (1.0) over (0.9) ...",
                "Setting up foo (1.0) ...",
            ],
        );
        assert_eq!(got[1], None); // summary alone is not progress
        assert_eq!(got[3], Some(0.5 * (1.0 / 3.0)));
        assert_eq!(got[4], Some(0.5 * (2.0 / 3.0)));
        assert_eq!(got[5], Some(0.5 * (2.0 / 3.0)));
        // tx expected = 3*2 = 6 events; "Preparing to unpack" is not one.
        assert!((got[7].unwrap() - (0.5 * (2.0 / 3.0) + (1.0 - 0.5 * (2.0 / 3.0)) * (1.0 / 6.0)))
            .abs()
            < 1e-9);
        assert!(got[8].unwrap() > got[7].unwrap());
        assert!(got[8].unwrap() < 1.0);
    }

    #[test]
    fn pacman_counts_only_the_commit_phase() {
        let mut t = Tracker::new(&argv(&["pkexec", "pacman", "-S", "--noconfirm", "foo"]));
        let got = feed(
            &mut t,
            &[
                "(1/2) checking keys in keyring",
                "(2/2) checking package integrity",
                "(1/5) installing foo",
                "(3/5) installing bar",
                "(5/5) installing baz",
            ],
        );
        assert_eq!(got, vec![None, None, Some(0.2), Some(0.6), Some(1.0)]);
    }

    #[test]
    fn zypper_counts_done_lines_against_summary() {
        let mut t = Tracker::new(&argv(&[
            "pkexec",
            "zypper",
            "--non-interactive",
            "install",
            "foo",
        ]));
        let got = feed(
            &mut t,
            &[
                "The following 2 packages are going to be installed:",
                "Downloading: foo-1.0-1.1.x86_64 ................................ [done (1.2 MiB/s)]",
                "Downloading: bar-2.0-1.1.x86_64 ................................ [done (1.1 MiB/s)]",
                "Installing: foo-1.0-1.1.x86_64 ................................ [done]",
                "Installing: bar-2.0-1.1.x86_64 ................................ [done]",
            ],
        );
        assert_eq!(got[0], None);
        assert_eq!(got[1], Some(0.25));
        assert_eq!(got[2], Some(0.5));
        assert_eq!(got[3], Some(0.75));
        assert_eq!(got[4], Some(1.0));
    }

    // ── chained "update all" runs ────────────────────────────────────────────

    #[test]
    fn chained_markers_slice_the_arc_per_tool() {
        let mut t = Tracker::new(&argv(&[
            "sh",
            "-c",
            "echo __spotty_part_1_2_flatpak__ && flatpak update --assumeyes && echo __spotty_part_2_2_dnf__ && pkexec dnf upgrade -y",
        ]));
        assert_eq!(t.fraction(), None);
        assert!(is_part_marker("__spotty_part_1_2_flatpak__"));
        assert!(!is_part_marker("Installing 1/2…"));

        t.feed("__spotty_part_1_2_flatpak__");
        assert_eq!(t.fraction(), None); // first part, nothing done yet

        t.feed("Updating 1/2…                        50%  1.2 MB/s");
        assert_eq!(t.fraction(), Some(0.125)); // flatpak 25% done × part 1 of 2

        t.feed("Updating 1/2… ████████████████████ 100%");
        assert_eq!(t.fraction(), Some(0.25)); // flatpak op 1 of 2 complete

        t.feed("Updating 2/2… ████████████████████ 100%");
        assert_eq!(t.fraction(), Some(0.5)); // whole flatpak part complete

        t.feed("__spotty_part_2_2_dnf__");
        assert_eq!(t.fraction(), Some(0.5)); // part 2 starts where part 1 ended

        t.feed("[1/4] foo-1.0-1.fc44.x86_64            100% | 1.0 MiB/s | 1.0 MiB | 00m01s");
        assert!((t.fraction().unwrap() - (1.0 + 0.125) / 2.0).abs() < 1e-9);

        t.feed("Running transaction");
        t.feed("[1/2] Installing bar-2.0-1.fc44.x86_64 100% | 1.0 MiB/s | 1.0 MiB | 00m01s");
        // sub: base = 0.5*0.25, tx = 1/2 → 0.125 + 0.875*0.5
        let sub = 0.125 + 0.875 * 0.5;
        assert!((t.fraction().unwrap() - (1.0 + sub) / 2.0).abs() < 1e-9);

        t.feed("[2/2] Installing baz-3.0-1.fc44.x86_64 100% | 1.0 MiB/s | 1.0 MiB | 00m01s");
        assert_eq!(t.fraction(), Some(1.0));
    }

    // ── tools we can't interpret ─────────────────────────────────────────────

    #[test]
    fn unknown_and_snap_tools_keep_the_legacy_percent_reader() {
        let mut t = Tracker::new(&argv(&["some-thing", "--go"]));
        let got = feed(&mut t, &["working… 42%", "nothing here"]);
        assert_eq!(got, vec![Some(0.42), Some(0.42)]);

        let mut t = Tracker::new(&argv(&["pkexec", "snap", "install", "foo"]));
        let got = feed(&mut t, &["Fetching snap \"foo\"", "Download snap \"foo\" 80%"]);
        assert_eq!(got, vec![None, Some(0.8)]);
    }

    #[test]
    fn tiny_fractions_stay_indeterminate() {
        let mut t = Tracker::new(&argv(&["pkexec", "dnf", "install", "-y", "x"]));
        t.feed("[1/1000] pkg-1.0-1.fc44.x86_64         100% | 1.0 MiB/s | 1.0 MiB | 00m01s");
        // 0.5 * 0.001 = 0.0005 → below the drawing threshold → spinner.
        assert_eq!(t.fraction(), None);
    }
}
