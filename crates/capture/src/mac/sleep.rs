//! System sleep and wake, from `IOKit`'s root power domain. The kernel waits up to 30 s for each
//! process registered here to allow a sleep, so the helper closes its segment first: a sleep
//! never cuts a WAV mid-write. Mach host time stops while the Mac sleeps, so a segment that ran
//! across a sleep would hide the sleep's length; the next segment starts on a fresh clock
//! reading instead.
//!
//! A dark wake (Power Nap, a network wake) powers the CPU but not the graphics. The helper stays
//! idle through one: it reports the wake only once the system has its graphics capability.
//!
//! It also reads whether the lid is closed. Unsafe code: `IOKit`'s C calls, the callback's raw
//! arguments, and a dlsym of the power capability call.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError, mpsc};
use std::time::Duration;

use cidre::cf;

use crate::CaptureError;

/// Under the kernel's 30 s, so this process never makes the Mac wait the full limit.
const SLEEP_ACK_LIMIT: Duration = Duration::from_secs(20);

const MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
const MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
const MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;
/// `kIOPMCapabilityVideo` in `IOPMLibPrivate.h`: set on a full wake, clear on a dark wake.
const CAPABILITY_GRAPHICS: u32 = 0x2;
/// How often a dark wake checks whether it became a full wake. A dark wake moves to a full one
/// with no further power message, so this is the one check on a timer, and only while dark.
const FULL_WAKE_CHECK: Duration = Duration::from_secs(1);
const CAPABILITIES_SYMBOL: &CStr = c"IOPMConnectionGetSystemCapabilities";
const ROOT_DOMAIN_CLASS: &CStr = c"IOPMrootDomain";
const CLAMSHELL_KEY: &CStr = c"AppleClamshellState";
/// `RTLD_DEFAULT`: search every image the process loaded.
const RTLD_DEFAULT: *mut c_void = -2_isize as *mut c_void;
const UTF8: u32 = 0x0800_0100;

/// The root power domain connection the callback answers on.
static ROOT_PORT: AtomicU32 = AtomicU32::new(0);
type PowerCallback = Box<dyn Fn(PowerEvent) + Send + Sync>;

static ON_EVENT: OnceLock<PowerCallback> = OnceLock::new();
/// Set while a dark wake waits to become a full one. A sleep clears it.
static IS_DARK_WAKE: AtomicBool = AtomicBool::new(false);
/// The callback waiting for registration, until it moves into `ON_EVENT`.
static PENDING_EVENT: Mutex<Option<PowerCallback>> = Mutex::new(None);

pub enum PowerEvent {
    /// The Mac is about to sleep. It sleeps once `SleepAck::allow` runs, the ack drops, or 20 s
    /// pass.
    WillSleep(SleepAck),
    /// The Mac woke and its devices are powered.
    Woke,
}

/// Holds a sleep until the segment is closed.
pub struct SleepAck {
    allowed: Option<mpsc::SyncSender<()>>,
}

impl SleepAck {
    /// An ack no sleep waits on. For debug builds' injected events.
    #[cfg(debug_assertions)]
    #[must_use]
    pub fn detached() -> Self {
        Self { allowed: None }
    }

    /// Lets the Mac sleep now.
    pub fn allow(self) {
        if let Some(allowed) = self.allowed {
            // The callback may have given up waiting; the sleep goes ahead either way.
            let _late = allowed.send(());
        }
    }
}

/// Starts a thread that hears sleep and wake for the process's life and hands each to
/// `on_event`. Registers once; a call after a registration succeeded returns at once, and a
/// call after one failed tries again.
///
/// # Errors
///
/// Fails when `IOKit` refuses the registration or the thread cannot start.
pub fn watch_system_power(
    on_event: impl Fn(PowerEvent) + Send + Sync + 'static,
) -> Result<(), CaptureError> {
    if ON_EVENT.get().is_some() {
        return Ok(());
    }
    *PENDING_EVENT.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(on_event));
    let (registered, registration) = mpsc::channel();
    std::thread::Builder::new()
        .name("power".to_owned())
        .spawn(move || run_power_loop(&registered))
        .map_err(|error| CaptureError::PowerWatch {
            reason: error.to_string(),
        })?;
    registration
        .recv()
        .map_err(|_| CaptureError::PowerWatch {
            reason: "the power thread stopped before it registered".to_owned(),
        })?
        .map_err(|reason| CaptureError::PowerWatch { reason })
}

fn run_power_loop(registered: &mpsc::Sender<Result<(), String>>) {
    let mut notify_port: *mut c_void = std::ptr::null_mut();
    let mut notifier: u32 = 0;
    // SAFETY: both out-pointers point at live locals. The callback takes no refcon.
    let root_port = unsafe {
        IORegisterForSystemPower(
            std::ptr::null_mut(),
            &raw mut notify_port,
            power_changed,
            &raw mut notifier,
        )
    };
    if root_port == 0 || notify_port.is_null() {
        let _unheard = registered.send(Err("IORegisterForSystemPower failed".to_owned()));
        return;
    }
    ROOT_PORT.store(root_port, Ordering::Release);
    // SAFETY: the port is live for the process's life, and so is its source.
    let source = unsafe { IONotificationPortGetRunLoopSource(notify_port) };
    let Some(source) = (unsafe { source.as_ref() }) else {
        let _unheard = registered.send(Err("the power port has no run loop source".to_owned()));
        return;
    };
    cf::RunLoop::current().add_src(source, cf::RunLoopMode::default());
    if let Some(on_event) = PENDING_EVENT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        // Only this thread sets it, once, so it was empty.
        let _set = ON_EVENT.set(on_event);
    }
    let _unheard = registered.send(Ok(()));
    tracing::info!("power.watching");
    cf::RunLoop::run();
}

extern "C" fn power_changed(
    _refcon: *mut c_void,
    _service: u32,
    message_type: u32,
    message_argument: *mut c_void,
) {
    // The argument is the notification's ID, passed back to allow the change.
    let notification_id = message_argument as isize;
    match message_type {
        MESSAGE_CAN_SYSTEM_SLEEP => allow_power_change(notification_id),
        MESSAGE_SYSTEM_WILL_SLEEP => {
            tracing::info!("power.will_sleep");
            IS_DARK_WAKE.store(false, Ordering::Release);
            let (allowed, is_allowed) = mpsc::sync_channel(1);
            if let Some(on_event) = ON_EVENT.get() {
                on_event(PowerEvent::WillSleep(SleepAck {
                    allowed: Some(allowed),
                }));
            }
            match is_allowed.recv_timeout(SLEEP_ACK_LIMIT) {
                Ok(()) => tracing::info!("power.sleep_allowed"),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::info!("power.sleep_ack_dropped");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!(
                        power.ack_limit_ms =
                            u64::try_from(SLEEP_ACK_LIMIT.as_millis()).unwrap_or(0),
                        "power.sleep_ack_timed_out"
                    );
                }
            }
            allow_power_change(notification_id);
        }
        MESSAGE_SYSTEM_HAS_POWERED_ON => powered_on(system_capabilities()),
        // Will-not-sleep, will-power-on and the rest need no answer.
        _ => {}
    }
}

/// Reports a full wake at once. A dark wake waits on its own thread until the graphics come
/// on, or until a sleep ends it.
fn powered_on(capabilities: Option<u32>) {
    // An unreadable capability set counts as a full wake: missing one costs a segment.
    let is_full_wake = capabilities.is_none_or(|bits| bits & CAPABILITY_GRAPHICS != 0);
    tracing::info!(
        power.capabilities = capabilities,
        power.is_full_wake = is_full_wake,
        "power.woke"
    );
    if is_full_wake {
        // A dark wake's watch thread must not report this wake a second time.
        IS_DARK_WAKE.store(false, Ordering::Release);
        report_woke();
        return;
    }
    if IS_DARK_WAKE.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("dark-wake".to_owned())
        .spawn(|| {
            while IS_DARK_WAKE.load(Ordering::Acquire) {
                std::thread::sleep(FULL_WAKE_CHECK);
                let has_graphics =
                    system_capabilities().is_none_or(|bits| bits & CAPABILITY_GRAPHICS != 0);
                if has_graphics && IS_DARK_WAKE.swap(false, Ordering::AcqRel) {
                    tracing::info!("power.full_wake");
                    report_woke();
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(exception.message = %error, "power.dark_wake_thread_failed");
        IS_DARK_WAKE.store(false, Ordering::Release);
        report_woke();
    }
}

fn report_woke() {
    if let Some(on_event) = ON_EVENT.get() {
        on_event(PowerEvent::Woke);
    }
}

/// Runs the wake path as if `IOKit` had sent one, with the graphics on or off. For debug
/// builds' injected events: a dark wake then waits for the real capabilities, which are on.
#[cfg(debug_assertions)]
pub fn inject_powered_on(has_graphics: bool) {
    let graphics = if has_graphics { CAPABILITY_GRAPHICS } else { 0 };
    powered_on(Some(1 | graphics));
}

/// The system's power capabilities, through a private call. `None` when it is missing.
fn system_capabilities() -> Option<u32> {
    type Capabilities = unsafe extern "C" fn() -> u32;
    static CAPABILITIES: OnceLock<Option<Capabilities>> = OnceLock::new();
    let capabilities = (*CAPABILITIES.get_or_init(|| {
        // SAFETY: RTLD_DEFAULT is a valid handle and the name a valid C string.
        let symbol = unsafe { dlsym(RTLD_DEFAULT, CAPABILITIES_SYMBOL.as_ptr()) };
        if symbol.is_null() {
            tracing::warn!("power.capabilities_missing");
            return None;
        }
        // SAFETY: a non-null result for this symbol is IOKit's function, which takes nothing
        // and returns the capability bits.
        Some(unsafe { std::mem::transmute::<*mut c_void, Capabilities>(symbol) })
    }))?;
    // SAFETY: the function takes no arguments and only reads the power state.
    Some(unsafe { capabilities() })
}

/// Whether the lid is closed. A Mac that stays awake with its lid closed runs on an external
/// display, and macOS then mutes the built-in mic to exact zeros. `None` on a Mac with no lid.
#[must_use]
pub fn is_lid_closed() -> Option<bool> {
    // SAFETY: the class name is a valid C string; IOServiceMatching's dictionary is consumed by
    // IOServiceGetMatchingService.
    let root_domain =
        unsafe { IOServiceGetMatchingService(0, IOServiceMatching(ROOT_DOMAIN_CLASS.as_ptr())) };
    if root_domain == 0 {
        return None;
    }
    // SAFETY: the key is a valid UTF-8 C string; the string is released below.
    let key = unsafe { CFStringCreateWithCString(std::ptr::null(), CLAMSHELL_KEY.as_ptr(), UTF8) };
    if key.is_null() {
        // SAFETY: the service came from IOServiceGetMatchingService.
        unsafe { IOObjectRelease(root_domain) };
        return None;
    }
    // SAFETY: the service and the key are live. The property is released below.
    let property =
        unsafe { IORegistryEntryCreateCFProperty(root_domain, key, std::ptr::null(), 0) };
    // SAFETY: each object is released once.
    unsafe {
        CFRelease(key);
        IOObjectRelease(root_domain);
    }
    if property.is_null() {
        return None;
    }
    // SAFETY: the property is live; its type is checked before it is read as a boolean.
    let is_closed = unsafe {
        (CFGetTypeID(property) == CFBooleanGetTypeID()).then(|| CFBooleanGetValue(property) != 0)
    };
    // SAFETY: the property came from a Create call, so this process owns it.
    unsafe { CFRelease(property) };
    is_closed
}

fn allow_power_change(notification_id: isize) {
    let root_port = ROOT_PORT.load(Ordering::Acquire);
    // SAFETY: the port came from IORegisterForSystemPower and stays open; the ID came with the
    // message being answered.
    let status = unsafe { IOAllowPowerChange(root_port, notification_id) };
    if status != 0 {
        tracing::warn!(iokit.status = status, "power.allow_failed");
    }
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        notify_port: *mut *mut c_void,
        callback: extern "C" fn(*mut c_void, u32, u32, *mut c_void),
        notifier: *mut u32,
    ) -> u32;
    fn IONotificationPortGetRunLoopSource(notify_port: *mut c_void) -> *const cf::RunLoopSrc;
    fn IOAllowPowerChange(kernel_port: u32, notification_id: isize) -> i32;
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;
    fn IOObjectRelease(object: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        text: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFRelease(object: *const c_void);
    fn CFGetTypeID(object: *const c_void) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
}

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
