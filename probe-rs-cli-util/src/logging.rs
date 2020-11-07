use colored::*;
use env_logger::{Builder, Logger};
use indicatif::ProgressBar;
use log::{Level, LevelFilter, Log, Record};
use sentry::{
    integrations::panic::PanicIntegration,
    internals::{Dsn, Utc, Uuid},
    Breadcrumb,
};
use simplelog::{CombinedLogger, SharedLogger};
use std::{
    borrow::Cow,
    error::Error,
    fmt,
    io::Write,
    panic::PanicInfo,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
};

/// The maximum window width of the terminal, given in characters possible.
static MAX_WINDOW_WIDTH: AtomicUsize = AtomicUsize::new(0);

lazy_static::lazy_static! {
    /// Stores the progress bar for the logging facility.
    static ref PROGRESS_BAR: RwLock<Option<Arc<ProgressBar>>> = RwLock::new(None);
    static ref LOG: Arc<RwLock<Vec<Breadcrumb>>> = Arc::new(RwLock::new(vec![]));
}

/// A structure to hold a string with a padding attached to the start of it.
struct Padded<T> {
    value: T,
    width: usize,
}

impl<T: fmt::Display> fmt::Display for Padded<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{: <width$}", self.value, width = self.width)
    }
}

/// Get the maximum between the window width and the length of the given string.
fn max_target_width(target: &str) -> usize {
    let max_width = MAX_WINDOW_WIDTH.load(Ordering::Relaxed);
    if max_width < target.len() {
        MAX_WINDOW_WIDTH.store(target.len(), Ordering::Relaxed);
        target.len()
    } else {
        max_width
    }
}

/// Helper to receive a color for a given level.
fn colored_level(level: Level) -> ColoredString {
    match level {
        Level::Trace => "TRACE".magenta().bold(),
        Level::Debug => "DEBUG".blue().bold(),
        Level::Info => " INFO".green().bold(),
        Level::Warn => " WARN".yellow().bold(),
        Level::Error => "ERROR".red().bold(),
    }
}

struct ShareableLogger(Logger);

impl Log for ShareableLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.0.filter()
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0.log(record);
        }
    }

    fn flush(&self) {
        self.0.flush();
    }
}

impl SharedLogger for ShareableLogger {
    fn level(&self) -> LevelFilter {
        self.0.filter()
    }

    fn config(&self) -> Option<&simplelog::Config> {
        todo!()
    }

    fn as_log(self: Box<Self>) -> Box<dyn log::Log> {
        todo!()
    }
}

/// Initialize the logger.
///
/// There are two sources for log level configuration:
///
/// - The log level value passed to this function
/// - The user can set the `RUST_LOG` env var, which overrides the log level passed to this function.
///
/// The config file only accepts a log level, while the `RUST_LOG` variable
/// supports the full `env_logger` syntax, including filtering by crate and
/// module.
pub fn init(level: Option<Level>) {
    // User visible logging.

    let mut log_builder = Builder::new();

    // First, apply the log level given to this function.
    if let Some(level) = level {
        log_builder.filter_level(level.to_level_filter());
    } else {
        log_builder.filter_level(LevelFilter::Warn);
    }

    // Then override that with the `RUST_LOG` env var, if set.
    if let Ok(s) = ::std::env::var("RUST_LOG") {
        log_builder.parse_filters(&s);
    }

    // Define our custom log format.
    log_builder.format(move |f, record| {
        let target = record.target();
        let max_width = max_target_width(target);

        let level = colored_level(record.level());

        let mut style = f.style();
        let target = style.set_bold(true).value(Padded {
            value: target,
            width: max_width,
        });

        let guard = PROGRESS_BAR.write().unwrap();
        if let Some(pb) = &*guard {
            pb.println(format!("       {} {} > {}", level, target, record.args()));
        } else {
            println!("       {} {} > {}", level, target, record.args());
        }

        Ok(())
    });

    // Sentry logging (all log levels except tracing (to not clog the server disk & internet sink)).

    let mut sentry = Builder::new();

    // Always use the Debug log level.
    sentry.filter_level(LevelFilter::Debug);

    // Define our custom log format.
    sentry.format(move |_f, record| {
        let mut log_guard = LOG.write().unwrap();
        log_guard.push(Breadcrumb {
            level: match record.level() {
                Level::Error => sentry::Level::Error,
                Level::Warn => sentry::Level::Warning,
                Level::Info => sentry::Level::Info,
                Level::Debug => sentry::Level::Debug,
                Level::Trace => sentry::Level::Debug,
            },
            category: Some(record.target().to_string()),
            message: Some(format!("{}", record.args())),
            timestamp: Utc::now(),
            ..Default::default()
        });

        Ok(())
    });

    CombinedLogger::init(vec![
        Box::new(ShareableLogger(log_builder.build())),
        Box::new(ShareableLogger(sentry.build())),
    ])
    .unwrap();
}

/// Sets the currently displayed progress bar of the CLI.
pub fn set_progress_bar(progress: Arc<ProgressBar>) {
    let mut guard = PROGRESS_BAR.write().unwrap();
    *guard = Some(progress);
}

/// Disables the currently displayed progress bar of the CLI.
pub fn clear_progress_bar() {
    let mut guard = PROGRESS_BAR.write().unwrap();
    *guard = None;
}

fn send_logs() {
    let mut log_guard = LOG.write().unwrap();

    for breadcrumb in log_guard.drain(..) {
        sentry::add_breadcrumb(breadcrumb);
    }
}

fn sentry_config(release: String) -> sentry::ClientOptions {
    sentry::ClientOptions {
        dsn: Some(
            Dsn::from_str("https://4396a23b463a46b8b3bfa883910333fe@sentry.technokrat.ch/7")
                .unwrap(),
        ),
        release: Some(Cow::<'static>::Owned(release.to_string())),
        #[cfg(debug_assertions)]
        environment: Some(Cow::Borrowed("Development")),
        #[cfg(not(debug_assertions))]
        environment: Some(Cow::Borrowed("Production")),
        ..Default::default()
    }
}

pub struct Metadata {
    pub chip: Option<String>,
    pub probe: Option<String>,
    pub release: String,
}

/// Sets the metadata concerning the current probe-rs session on the sentry scope.
fn set_metadata(metadata: &Metadata) {
    sentry::configure_scope(|scope| {
        metadata
            .chip
            .as_ref()
            .map(|chip| scope.set_tag("chip", chip));
        metadata
            .probe
            .as_ref()
            .map(|probe| scope.set_tag("probe", probe));
    })
}

fn print_uuid(uuid: Uuid) {
    println(format!(
        "  {} {} {}",
        "Thank You!".cyan().bold(),
        "Your error was reported successfully. If you don't mind, please open an issue on Github and include the UUID: ",
        uuid
    ));
}

/// Captures an std::error::Error with sentry and sends all previously captured logs.
pub fn capture_error<E>(metadata: &Metadata, error: &E)
where
    E: Error + ?Sized,
{
    let _guard = sentry::init(sentry_config(metadata.release.clone()));
    set_metadata(metadata);
    send_logs();
    let uuid = sentry::capture_error(error);
    print_uuid(uuid);
}

/// Captures an anyhow error with sentry and sends all previously captured logs.
pub fn capture_anyhow(metadata: &Metadata, error: &anyhow::Error) {
    let _guard = sentry::init(sentry_config(metadata.release.clone()));
    set_metadata(metadata);
    send_logs();
    let uuid = sentry::integrations::anyhow::capture_anyhow(error);
    print_uuid(uuid);
}

/// Captures a panic with sentry and sends all previously captured logs.
pub fn capture_panic(metadata: &Metadata, info: &PanicInfo<'_>) {
    let _guard = sentry::init(sentry_config(metadata.release.clone()));
    set_metadata(metadata);
    send_logs();
    let uuid = sentry::with_integration(|integration: &PanicIntegration, hub| {
        hub.capture_event(integration.event_from_panic_info(info))
    });
    print_uuid(uuid);
}

/// Ask for a line of text.
fn text() -> std::io::Result<String> {
    // Read up to the first newline or EOF.

    let mut out = String::new();
    std::io::stdin().read_line(&mut out)?;

    // Only capture up to the first newline.
    if let Some(mut newline) = out.find('\n') {
        if newline > 0 && out.as_bytes()[newline - 1] == b'\r' {
            newline -= 1;
        }
        out.truncate(newline);
    }

    Ok(out)
}

/// Displays the text to ask if the crash should be reported.
pub fn ask_to_log_crash() -> bool {
    if let Ok(var) = std::env::var("PROBE_RS_SENTRY") {
        var == "true"
    } else {
        println(format!(
            "        {} {}",
            "Hint".blue().bold(),
            "Unfortunately probe-rs encountered an unhandled problem. To help the devs, you can automatically log the error to sentry.technokrat.ch."
        ));
        println(format!(
            "             {}",
            "Your data will be transmitted completely anonymous and cannot be associated with you directly."
        ));
        println(format!(
            "             {}",
            "To Hide this message in the future, please set $PROBE_RS_SENTRY to 'true' or 'false'."
        ));
        print!("             {}", "Do you wish to transmit the data? y/N: ");
        std::io::stdout().flush().ok();
        let result = if let Ok(s) = text() {
            let s = s.to_lowercase();
            if s.is_empty() {
                false
            } else if "yes".starts_with(&s) {
                true
            } else if "no".starts_with(&s) {
                false
            } else {
                false
            }
        } else {
            false
        };

        println!();

        result
    }
}

/// Writes an error to stderr.
/// This function respects the progress bars of the CLI that might be displayed and displays the message above it if any are.
pub fn eprintln(message: impl AsRef<str>) {
    if let Ok(guard) = PROGRESS_BAR.try_write() {
        match guard.as_ref() {
            Some(pb) if !pb.is_finished() => {
                pb.println(message.as_ref());
            }
            _ => {
                eprintln!("{}", message.as_ref());
            }
        }
    } else {
        eprintln!("{}", message.as_ref());
    }
}

/// Writes a message to stdout with a newline at the end.
/// This function respects the progress bars of the CLI that might be displayed and displays the message above it if any are.
pub fn println(message: impl AsRef<str>) {
    if let Ok(guard) = PROGRESS_BAR.try_write() {
        match guard.as_ref() {
            Some(pb) if !pb.is_finished() => {
                pb.println(message.as_ref());
            }
            _ => {
                println!("{}", message.as_ref());
            }
        }
    } else {
        println!("{}", message.as_ref());
    }
}