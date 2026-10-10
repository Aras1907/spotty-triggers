//! Copy all of a login, one paste at a time: the clipboard side of the paste
//! sequence. The steps and their timing are in `proton_pass_sequence`.
//!
//! The clipboard holds a `SequenceProvider`, which offers one value at a time.
//! When another app pastes it, GDK asks the provider to write the value into
//! that app's stream, and that write is the paste: the sequence then offers the
//! next step once the reads have been quiet for a moment. A write while a Spotty
//! window has focus does not count (typing a paste into Spotty's own search field
//! is not a paste into another app), and format queries never write, so they
//! never advance anything.
//!
//! The provider carries the password-manager hint, so Spotty's clipboard history
//! skips it before it reads any text (`clipboard.rs`), and its storable formats
//! are empty, so a clipboard manager never keeps the values.
//!
//! Known limitation: an app that reads the clipboard without pasting (a clipboard
//! manager that ignores the hint) counts as a paste and advances the sequence.
//! The sequence ends after 60 s, when Pass locks, or when something else is
//! copied, and then clears the clipboard if it still holds the sequence.

use crate::i18n::gettext;
use crate::proton_pass;
use crate::proton_pass_pin;
use crate::proton_pass_sequence::{self as seq, LoginValues, Plan, Step, Tick};
use futures::channel::oneshot;
use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};
use spotty_proton_account::{Zeroize, Zeroizing};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// The password-manager hint (KDE's convention, honoured by clipboard managers).
const HINT: &str = "x-kde-passwordManagerHint";
const TEXT_UTF8: &str = "text/plain;charset=utf-8";
/// How often the sequence checks that Pass is unlocked and its lifetime runs on.
const WATCH: Duration = Duration::from_secs(1);

/// Bytes that may be secret, handed to one reader. Wiped when dropped.
struct Wiped(Vec<u8>);

impl AsRef<[u8]> for Wiped {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Wiped {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// The sequence in progress. Its values are held only until their step is offered.
struct Active {
    id: u64,
    /// The provider on the clipboard now. A write for any other offer is stale.
    offer: u64,
    plan: Plan,
    /// The username and password for each step; each is taken when its step is offered.
    values: Vec<Option<Zeroizing<String>>>,
    target: String,
    clipboard: gdk::Clipboard,
    changed: Option<glib::SignalHandlerId>,
    /// A debounce check is scheduled.
    timer_pending: bool,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    static LAST_NUMBER: Cell<u64> = const { Cell::new(0) };
}

/// How a sequence ended. Only the way it ended is said, never a value.
enum Outcome {
    Done,
    Expired,
    Locked,
    Changed,
    Failed(String),
    /// A new sequence took over; nothing is said about this one.
    Replaced,
}

fn next_number() -> u64 {
    LAST_NUMBER.with(|last| {
        let next = last.get() + 1;
        last.set(next);
        next
    })
}

/// Start copying a login one paste at a time. The caller has checked the PIN.
/// GTK main thread only.
pub fn start(login: LoginValues) {
    let LoginValues { target, mut username, mut password, has_totp } = login;
    let steps = seq::steps_for(username.is_some(), password.is_some(), has_totp);
    if steps.is_empty() {
        proton_pass::notify(&gettext("Proton Pass"), &gettext("This item has no username, password or one-time code to paste."));
        return;
    }
    let Some(display) = gdk::Display::default() else { return };
    end(Outcome::Replaced);
    let values = steps
        .iter()
        .map(|step| match step {
            Step::Username => username.take(),
            Step::Password => password.take(),
            Step::Code => None,
        })
        .collect();
    let clipboard = display.clipboard();
    let changed = clipboard.connect_changed(on_changed);
    let id = next_number();
    let order = steps.iter().map(|step| step.label()).collect::<Vec<_>>().join(gettext(", then ").as_str());
    ACTIVE.with(|active| {
        *active.borrow_mut() = Some(Active {
            id,
            offer: 0,
            plan: Plan::new(steps, Instant::now()),
            values,
            target,
            clipboard: clipboard.clone(),
            changed: Some(changed),
            timer_pending: false,
        })
    });
    proton_pass::notify(
        &gettext("Paste sequence started"),
        &gettext("Paste {steps}. The clipboard moves on after each paste and clears after {seconds} s.")
            .replace("{steps}", &order)
            .replace("{seconds}", &seq::LIFETIME.as_secs().to_string()),
    );
    glib::timeout_add_local(WATCH, move || watch(id));
    offer_current();
}

/// Put the current step's value on the clipboard.
fn offer_current() {
    let offered = ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        let active = active.as_mut()?;
        let index = active.plan.index();
        let step = active.plan.current()?;
        let value = active.values.get_mut(index).and_then(Option::take);
        let offer = next_number();
        active.offer = offer;
        Some((SequenceProvider::new(offer, step, value, active.target.clone()), active.clipboard.clone()))
    });
    let Some((provider, clipboard)) = offered else { return };
    proton_pass::supersede_copy_clear();
    if clipboard.set_content(Some(&provider)).is_err() {
        end(Outcome::Failed(gettext("The clipboard didn't accept the value.")));
    }
}

/// The clipboard changed. Anything other than this sequence's current offer
/// ends the sequence.
fn on_changed(clipboard: &gdk::Clipboard) {
    let Some(expected) = ACTIVE.with(|active| active.borrow().as_ref().map(|active| active.offer)) else {
        return;
    };
    let ours = clipboard.content().and_then(|content| content.downcast::<SequenceProvider>().ok()).map(|provider| provider.offer());
    if ours != Some(expected) {
        end(Outcome::Changed);
    }
}

/// Whether one of Spotty's windows has focus. A paste then went into Spotty
/// itself (its search field, say), not into another app.
fn spotty_has_focus() -> bool {
    let windows = gtk::Window::toplevels();
    (0..windows.n_items())
        .filter_map(|index| windows.item(index))
        .filter_map(|object| object.downcast::<gtk::Window>().ok())
        .any(|window| window.is_active())
}

/// A text value of `offer` has reached another app.
fn note_read(offer: u64) {
    if spotty_has_focus() {
        return;
    }
    let due = ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        let active = active.as_mut()?;
        if active.offer != offer || active.plan.finished() {
            return None;
        }
        active.plan.note_read(Instant::now());
        if active.timer_pending {
            return None;
        }
        active.timer_pending = true;
        Some(active.id)
    });
    if let Some(id) = due {
        schedule(id, seq::QUIET);
    }
}

fn schedule(id: u64, delay: Duration) {
    glib::timeout_add_local_once(delay, move || step(id));
}

/// The debounce check: advance once the reads of the current step have gone quiet.
fn step(id: u64) {
    let tick = ACTIVE.with(|active| {
        let mut active = active.borrow_mut();
        let active = active.as_mut().filter(|active| active.id == id)?;
        let tick = active.plan.tick(Instant::now());
        active.timer_pending = matches!(tick, Tick::Wait(_));
        Some(tick)
    });
    match tick {
        None | Some(Tick::Idle) => {}
        Some(Tick::Wait(delay)) => schedule(id, delay),
        Some(Tick::Expired) => end(Outcome::Expired),
        Some(Tick::Advance) => {
            let finished = ACTIVE.with(|active| {
                let mut active = active.borrow_mut();
                let active = active.as_mut().filter(|active| active.id == id)?;
                active.plan.advance();
                Some(active.plan.finished())
            });
            match finished {
                Some(true) => end(Outcome::Done),
                Some(false) => offer_current(),
                None => {}
            }
        }
    }
}

/// Once a second: end the sequence when Pass locks or its lifetime runs out.
fn watch(id: u64) -> glib::ControlFlow {
    let expired = ACTIVE.with(|active| {
        active.borrow().as_ref().filter(|active| active.id == id).map(|active| active.plan.expired(Instant::now()))
    });
    match expired {
        None => glib::ControlFlow::Break,
        Some(true) => {
            end(Outcome::Expired);
            glib::ControlFlow::Break
        }
        Some(false) if !proton_pass_pin::unlocked() => {
            end(Outcome::Locked);
            glib::ControlFlow::Break
        }
        Some(false) => glib::ControlFlow::Continue,
    }
}

/// A write for `offer` failed: the sequence stops with `message`.
fn fail(offer: u64, message: String) {
    let current = ACTIVE.with(|active| active.borrow().as_ref().is_some_and(|active| active.offer == offer));
    if current {
        end(Outcome::Failed(message));
    }
}

/// End the sequence. The clipboard is cleared if it still holds the sequence's
/// value (and no new sequence has taken over). Values are wiped as `active` drops.
fn end(outcome: Outcome) {
    let Some(mut active) = ACTIVE.with(|slot| slot.borrow_mut().take()) else { return };
    if let Some(handler) = active.changed.take() {
        active.clipboard.disconnect(handler);
    }
    let ours = active
        .clipboard
        .content()
        .and_then(|content| content.downcast::<SequenceProvider>().ok())
        .is_some_and(|provider| provider.offer() == active.offer);
    let cleared = ours && !matches!(outcome, Outcome::Changed | Outcome::Replaced);
    if cleared {
        let _ = active.clipboard.set_content(None::<&gdk::ContentProvider>);
    }
    let (title, body) = match outcome {
        Outcome::Replaced => return,
        Outcome::Done => (gettext("Paste sequence done"), gettext("Every value is pasted. The clipboard is cleared.")),
        Outcome::Expired => (gettext("Paste sequence ended"), gettext("It ran out of time, so the clipboard is cleared.")),
        Outcome::Locked if cleared => (gettext("Paste sequence stopped"), gettext("Proton Pass locked. The clipboard is cleared.")),
        Outcome::Locked => (gettext("Paste sequence stopped"), gettext("Proton Pass locked.")),
        Outcome::Changed => (gettext("Paste sequence stopped"), gettext("Something else was copied, so the sequence ended.")),
        Outcome::Failed(message) => (gettext("Paste sequence stopped"), message),
    };
    proton_pass::notify(&title, &body);
}

/// The text formats a paste may ask for.
fn is_text(mime: &str) -> bool {
    mime.starts_with("text/plain") || matches!(mime, "UTF8_STRING" | "STRING" | "TEXT")
}

/// Write `bytes` to the reader's stream.
async fn write_all(stream: gio::OutputStream, bytes: Wiped, priority: glib::Priority) -> Result<(), glib::Error> {
    match stream.write_all_future(bytes, priority).await {
        Ok((_, _, None)) => Ok(()),
        Ok((_, _, Some(error))) | Err((_, error)) => Err(error),
    }
}

/// The one-time code of the item, read now: a code step is read when it is
/// pasted, so the code is the one current at that moment.
async fn read_code(target: String) -> Result<Wiped, String> {
    if !proton_pass_pin::unlocked() {
        return Err(gettext("Proton Pass is locked."));
    }
    let (sender, receiver) = oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(proton_pass::totp_now(&target));
    });
    match receiver.await {
        Ok(Ok(code)) => Ok(Wiped(code.as_bytes().to_vec())),
        Ok(Err(message)) => Err(message),
        Err(_) => Err(gettext("The one-time code couldn't be read.")),
    }
}

/// Answer one read of the provider's `offer`: a text format serves the step's
/// value (a code step reads it first), and a served read is the paste.
async fn serve(
    offer: u64,
    step: Option<Step>,
    value: Option<Wiped>,
    target: String,
    stream: gio::OutputStream,
    priority: glib::Priority,
) -> Result<(), glib::Error> {
    if !proton_pass_pin::unlocked() {
        // Never serve a value while locked, even before the watch has ended the sequence.
        let message = gettext("Proton Pass is locked.");
        fail(offer, message.clone());
        return Err(glib::Error::new(gio::IOErrorEnum::Failed, &message));
    }
    let bytes = match (step, value) {
        (Some(Step::Code), _) => match read_code(target).await {
            Ok(bytes) => bytes,
            Err(message) => {
                fail(offer, message.clone());
                return Err(glib::Error::new(gio::IOErrorEnum::Failed, &message));
            }
        },
        (_, Some(bytes)) => bytes,
        _ => return Err(glib::Error::new(gio::IOErrorEnum::Failed, &gettext("Nothing to paste."))),
    };
    write_all(stream, bytes, priority).await?;
    note_read(offer);
    Ok(())
}

/// Answer the password-manager hint. It asks whether the value is a password;
/// it is not a paste.
async fn answer_hint(stream: gio::OutputStream, priority: glib::Priority) -> Result<(), glib::Error> {
    write_all(stream, Wiped(b"secret".to_vec()), priority).await
}

async fn refuse(message: String) -> Result<(), glib::Error> {
    Err(glib::Error::new(gio::IOErrorEnum::NotSupported, &message))
}

mod imp {
    use super::*;

    /// One clipboard value of a sequence: a username, a password or a code.
    #[derive(Default)]
    pub struct SequenceProvider {
        pub(super) offer: Cell<u64>,
        pub(super) step: Cell<Option<Step>>,
        pub(super) value: RefCell<Option<Zeroizing<String>>>,
        pub(super) target: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SequenceProvider {
        const NAME: &'static str = "SpottyProtonPassSequenceProvider";
        type Type = super::SequenceProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for SequenceProvider {}

    impl gdk::subclass::content_provider::ContentProviderImpl for SequenceProvider {
        fn formats(&self) -> gdk::ContentFormats {
            // Text formats named outright: GDK answers another app's paste of a
            // listed MIME type through `write_mime_type_future`, which is what
            // counts a paste. With only the string GType listed, GDK would
            // serialise `value()` instead and no paste would ever be seen.
            // The GType stays for reads inside Spotty, which use `value()`.
            gdk::ContentFormats::new(&[TEXT_UTF8, "text/plain", "UTF8_STRING", "STRING", "TEXT", HINT])
                .union(&gdk::ContentFormats::for_type(<String as glib::prelude::StaticType>::static_type()))
        }

        fn storable_formats(&self) -> gdk::ContentFormats {
            // A clipboard manager never keeps these values.
            gdk::ContentFormats::new(&[])
        }

        fn value(&self, type_: glib::Type) -> Result<glib::Value, glib::Error> {
            // A read made inside Spotty takes the text directly: it is not a paste.
            let text = self.value.borrow();
            match (type_ == <String as glib::prelude::StaticType>::static_type(), self.step.get(), text.as_ref()) {
                (true, Some(Step::Username | Step::Password), Some(text)) => Ok(glib::Value::from(text.as_str())),
                _ => Err(glib::Error::new(gio::IOErrorEnum::NotSupported, "This value can't be read directly.")),
            }
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let stream = stream.clone();
            if mime_type == HINT {
                return Box::pin(answer_hint(stream, io_priority));
            }
            if !is_text(mime_type) {
                return Box::pin(refuse(gettext("This format isn't offered.")));
            }
            let value = self.value.borrow().as_ref().map(|text| Wiped(text.as_bytes().to_vec()));
            Box::pin(serve(
                self.offer.get(),
                self.step.get(),
                value,
                self.target.borrow().clone(),
                stream,
                io_priority,
            ))
        }
    }
}

glib::wrapper! {
    /// A clipboard value offered one step at a time (see the module docs).
    pub struct SequenceProvider(ObjectSubclass<imp::SequenceProvider>) @extends gdk::ContentProvider;
}

impl SequenceProvider {
    fn new(offer: u64, step: Step, value: Option<Zeroizing<String>>, target: String) -> Self {
        let provider: Self = glib::Object::new();
        let imp = provider.imp();
        imp.offer.set(offer);
        imp.step.set(Some(step));
        *imp.value.borrow_mut() = value;
        *imp.target.borrow_mut() = target;
        provider
    }

    fn offer(&self) -> u64 {
        self.imp().offer.get()
    }
}
