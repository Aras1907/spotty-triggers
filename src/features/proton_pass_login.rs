//! Signing in to Proton Pass with its own password, through `pass-cli`.
//!
//! Proton Pass has its own sign-in, separate from every other Proton app.
//! `pass-cli login --interactive <email>` is the password login of Proton's
//! client: it asks for the password on its terminal (its prompt is read from
//! `/dev/tty`), then for a two-factor code or a mailbox password when the
//! account needs one. Spotty runs the client inside a pseudo-terminal and
//! answers those prompts itself:
//!
//! - the email goes on the command line (it is not a secret);
//! - the password, two-factor code and mailbox password go only to the
//!   terminal's input, never to argv or the environment;
//! - the child runs with the same cleared environment as every other `pass-cli`
//!   call (`crate::proton_pass::command`), and gets its own session folder.
//!
//! Nothing typed here is stored. Each secret lives in a `Zeroizing` buffer for
//! the moment it is sent, and the output of the client (which never contains a
//! secret) is dropped as soon as it has been read.
//!
//! Every function here blocks: call them off the GTK thread.

use crate::proton_pass;
use spotty_proton_account::Zeroizing;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the client may take to ask for the next thing.
const PROMPT_WAIT: Duration = Duration::from_secs(90);

/// What the sign-in needs next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Done,
    TwoFactor,
    MailboxPassword,
    /// Proton Pass's own extra password, set in Pass's settings.
    ExtraPassword,
    Failed(String),
}

/// A sign-in the client is still running.
struct Login {
    child: Child,
    /// The terminal's master side: what Spotty types goes in here.
    input: File,
    /// Everything the client printed since the last prompt was answered.
    output: Receiver<Vec<u8>>,
}

static LOGIN: Mutex<Option<Login>> = Mutex::new(None);

fn slot() -> std::sync::MutexGuard<'static, Option<Login>> {
    LOGIN.lock().unwrap_or_else(|p| p.into_inner())
}

/// Start a sign-in with `email` and `password`. Replaces an earlier one.
pub fn begin(email: &str, password: Zeroizing<String>) -> Step {
    cancel();
    if !valid_email(email) {
        return Step::Failed("Type your Proton email or username.".into());
    }
    // A session left half done (an earlier sign-in that never finished)
    // makes the client refuse a new one; clear it first.
    proton_pass::clear_unfinished_session();
    let mut login = match start(email) {
        Ok(login) => login,
        Err(message) => return Step::Failed(message),
    };
    // First the password prompt, then whatever comes after it.
    let step = match wait_for_prompt(&mut login) {
        Prompt::Asked(text) if text.to_lowercase().contains("password") => {
            send(&mut login, password);
            next_step(&mut login)
        }
        Prompt::Asked(_) => Step::Failed("Proton Pass asked for something Spotty doesn't know how to answer.".into()),
        other => finish_without_prompt(&mut login, other),
    };
    keep_or_drop(login, &step);
    step
}

/// Answer the two-factor prompt with `code`.
pub fn submit_code(code: Zeroizing<String>) -> Step {
    with_login(|login| {
        send(login, code);
        next_step(login)
    })
}

/// Answer the mailbox-password or Pass extra-password prompt.
pub fn submit_mailbox(password: Zeroizing<String>) -> Step {
    with_login(|login| {
        send(login, password);
        next_step(login)
    })
}

/// Stop the sign-in in progress, if any.
pub fn cancel() {
    if let Some(mut login) = slot().take() {
        let _ = login.child.kill();
        let _ = login.child.wait();
    }
}

fn with_login(f: impl FnOnce(&mut Login) -> Step) -> Step {
    let Some(mut login) = slot().take() else {
        return Step::Failed("The sign-in expired. Enter your password again.".into());
    };
    let step = f(&mut login);
    keep_or_drop(login, &step);
    step
}

/// A sign-in that needs more input stays in the slot; any other end stops it.
/// After a success the client is given a moment to finish saving its session
/// before it is stopped.
fn keep_or_drop(mut login: Login, step: &Step) {
    match step {
        Step::TwoFactor | Step::MailboxPassword | Step::ExtraPassword => *slot() = Some(login),
        Step::Done => {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if matches!(login.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = login.child.kill();
            let _ = login.child.wait();
        }
        Step::Failed(_) => {
            let _ = login.child.kill();
            let _ = login.child.wait();
        }
    }
}

// ── Starting the client on a terminal ───────────────────────────────────────

/// An account name as pass-cli takes it: an email or a username. It must not
/// look like an option and must hold no whitespace or control characters.
fn valid_email(email: &str) -> bool {
    let email = email.trim();
    (1..=254).contains(&email.len()) && !email.starts_with('-') && email.chars().all(|c| !c.is_control() && !c.is_whitespace())
}

fn start(email: &str) -> Result<Login, String> {
    let mut command = proton_pass::command()?;
    command.args(["login", "--interactive", email.trim()]);
    let (master, slave) = open_terminal().map_err(|e| format!("Couldn't open a terminal for Proton Pass: {e}"))?;
    let stdout = slave.try_clone().map_err(|e| e.to_string())?;
    let stderr = slave.try_clone().map_err(|e| e.to_string())?;
    command.stdin(Stdio::from(slave)).stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr));
    // The client reads its prompts from /dev/tty, so the terminal must be its
    // controlling terminal: a new session, with the terminal made its own.
    // SAFETY: only async-signal-safe libc calls run between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|e| format!("Couldn't start the Proton Pass client: {e}"))?;
    let input = master.try_clone().map_err(|e| e.to_string())?;
    let (sender, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut master = master;
        let mut buffer = [0u8; 1024];
        loop {
            match master.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sender.send(buffer[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(Login { child, input, output })
}

/// A master/slave pair. The slave has echo off, so typed secrets never come back.
fn open_terminal() -> std::io::Result<(File, File)> {
    // SAFETY: plain libc calls on descriptors this function owns; every failure
    // closes what was opened so far.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if master < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let master = File::from_raw_fd(master);
        if libc::grantpt(master.as_raw_fd()) < 0 || libc::unlockpt(master.as_raw_fd()) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let name = libc::ptsname(master.as_raw_fd());
        if name.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if slave < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let slave = File::from_raw_fd(slave);
        let mut attributes: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(slave.as_raw_fd(), &mut attributes) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        attributes.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attributes) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((master, slave))
    }
}

// ── Talking to the prompts ──────────────────────────────────────────────────

/// What reading the terminal came to.
enum Prompt {
    /// The client asked for something matching the predicate; the text is its prompt.
    Asked(String),
    /// The client said it signed in.
    Success,
    /// The client stopped; the text is what it printed last.
    Ended(String),
    /// Nothing came for too long.
    TimedOut,
}

/// Read until the client asks for something, reports success or stops.
fn wait_for_prompt(login: &mut Login) -> Prompt {
    let deadline = Instant::now() + PROMPT_WAIT;
    let mut text = Zeroizing::new(String::new());
    loop {
        if let Some(prompt) = classify(&text) {
            return prompt;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match login.output.recv_timeout(left) {
            Ok(chunk) => text.push_str(&String::from_utf8_lossy(&chunk)),
            Err(RecvTimeoutError::Timeout) => return Prompt::TimedOut,
            Err(RecvTimeoutError::Disconnected) => {
                return match classify(&text) {
                    Some(prompt) => prompt,
                    None => Prompt::Ended(text.to_string()),
                };
            }
        }
    }
}

fn classify(text: &str) -> Option<Prompt> {
    if text.contains("Successfully logged in") {
        return Some(Prompt::Success);
    }
    // A prompt is the last, unfinished line, ending in a colon.
    let last = text.rsplit(['\n', '\r']).next().unwrap_or("").trim();
    if last.ends_with(':') {
        return Some(Prompt::Asked(last.to_owned()));
    }
    None
}

/// After the password: a two-factor code, a mailbox password, success or an error.
fn next_step(login: &mut Login) -> Step {
    match wait_for_prompt(login) {
        Prompt::Asked(text) => {
            let text = text.to_lowercase();
            if text.contains("extra password") {
                Step::ExtraPassword
            } else if text.contains("password") {
                Step::MailboxPassword
            } else if ["code", "2fa", "totp", "authenticator"].iter().any(|word| text.contains(word)) {
                Step::TwoFactor
            } else {
                Step::Failed("Proton Pass asked for something Spotty doesn't know how to answer.".into())
            }
        }
        other => finish_without_prompt(login, other),
    }
}

fn finish_without_prompt(login: &mut Login, prompt: Prompt) -> Step {
    match prompt {
        Prompt::Success => Step::Done,
        Prompt::TimedOut => Step::Failed("Proton Pass took too long to answer. Try again.".into()),
        Prompt::Ended(text) => {
            // The client ends after a success too; its exit status tells them apart.
            let status = login.child.wait().ok();
            if status.is_some_and(|s| s.success()) {
                return Step::Done;
            }
            Step::Failed(failure_text(&text))
        }
        Prompt::Asked(_) => Step::Failed("Proton Pass asked for something Spotty doesn't know how to answer.".into()),
    }
}

/// The last error line the client printed, without anything after it that
/// could be a prompt. Never includes what was typed.
fn failure_text(text: &str) -> String {
    // Proton's own reason, e.g. `error: "This email address does not exist…"`.
    if let Some(start) = text.find("error: \"") {
        let rest = &text[start + 8..];
        if let Some(end) = rest.find('"') {
            let reason = rest[..end].trim();
            if !reason.is_empty() {
                return reason.chars().take(300).collect();
            }
        }
    }
    // The client's own explanations, in its words.
    for known in [
        "Your account cannot login interactively",
        "FIDO-only accounts must use web login",
        "Too many incorrect extra password attempts",
        "Incorrect extra password",
    ] {
        if let Some(line) = text.lines().map(str::trim).find(|line| line.starts_with(known)) {
            return line.chars().take(300).collect();
        }
    }
    if text.contains("Authentication failed") {
        return "Proton Pass couldn't sign you in. Check your email and password.".into();
    }
    let line = text.lines().map(str::trim).rev().find(|line| line.to_lowercase().contains("error")).unwrap_or("");
    let line = line.strip_prefix("Error: ").unwrap_or(line);
    if line.is_empty() {
        "Proton Pass couldn't sign you in.".into()
    } else {
        line.chars().take(300).collect()
    }
}

/// Type `secret` and Enter into the terminal. The line is wiped after writing.
fn send(login: &mut Login, secret: Zeroizing<String>) {
    let mut line = Zeroizing::new(String::with_capacity(secret.len() + 1));
    line.push_str(&secret);
    line.push('\n');
    let _ = login.input.write_all(line.as_bytes());
    let _ = login.input.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names_that_could_be_options_or_contain_spaces_are_refused() {
        assert!(valid_email("sam@proton.test"));
        assert!(valid_email("sam"));
        assert!(!valid_email("-x@proton.test"));
        assert!(!valid_email("sam @proton.test"));
        assert!(!valid_email("sam\n@proton.test"));
        assert!(!valid_email(""));
    }

    #[test]
    fn the_last_unfinished_line_is_the_prompt() {
        match classify("Logging in...\nEnter password: ") {
            Some(Prompt::Asked(text)) => assert_eq!(text, "Enter password:"),
            _ => panic!("the password prompt should be recognised"),
        }
        // A finished line is not a prompt.
        assert!(classify("Enter password: done\n").is_none());
        assert!(matches!(classify("Successfully logged in as sam@proton.test\n"), Some(Prompt::Success)));
    }

    #[test]
    fn errors_are_reported_without_the_prompt_text() {
        assert_eq!(failure_text("Enter password: \nError: Incorrect login credentials\n"), "Incorrect login credentials");
        assert_eq!(failure_text("nothing useful"), "Proton Pass couldn't sign you in.");
        // Proton's reason wins over the client's generic last line.
        let real = "\r\nAuthentication failed: session flow: 422 res=http::Response { error_code: Some(ProtonBodyCode { code: 8002, error: \"This email address does not exist.\" }) } \r\nError: Error in interactive login flow\r\n";
        assert_eq!(failure_text(real), "This email address does not exist.");
        assert_eq!(
            failure_text("Your account cannot login interactively. Use web login instead\nError: x\n"),
            "Your account cannot login interactively. Use web login instead"
        );
    }

    /// The terminal plumbing: the child reads its input from /dev/tty, and
    /// what Spotty types arrives there. A shell stands in for pass-cli, so
    /// nothing touches the network.
    #[test]
    fn the_password_reaches_the_child_through_the_terminal() {
        use std::os::unix::process::CommandExt;
        let (master, slave) = open_terminal().expect("a terminal");
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "printf 'Enter password: '; IFS= read -r secret < /dev/tty; printf 'got %s\\n' \"$secret\""])
            .env_clear()
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("sh");
        drop(command);
        let input = master.try_clone().unwrap();
        let (sender, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut master = master;
            let mut buffer = [0u8; 256];
            while let Ok(n) = master.read(&mut buffer) {
                if n == 0 || sender.send(buffer[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut login = Login { child, input, output };
        assert!(matches!(wait_for_prompt(&mut login), Prompt::Asked(_)));
        send(&mut login, Zeroizing::new("hunter2".to_owned()));
        match wait_for_prompt(&mut login) {
            Prompt::Ended(text) => assert!(text.contains("got hunter2"), "child saw: {text:?}"),
            _ => panic!("the child should finish after reading the password"),
        }
        let _ = login.child.wait();
    }
}
