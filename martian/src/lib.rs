//! # Martian-Rust
//! This crate provides a strongly typed high level API for implementing martian stages in Rust.
//!
//! ## Documentation
//! For a guide style documentation and examples, visit: [https://martian-lang.github.io/martian-rust/](https://martian-lang.github.io/martian-rust/#/)

pub use anyhow::Error;
use anyhow::{ensure, Context, Result};
use backtrace::Backtrace;
use log::{error, info};
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::Write as IoWrite;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::path::Path;
use std::{io, panic};
use time::format_description::modifier::{Day, Hour, Minute, Month, Second, Year};
use time::format_description::FormatItem::Literal;
use time::format_description::{Component, FormatItem};
use time::OffsetDateTime;
use utils::current_executable;

mod metadata;
pub use metadata::*;

#[macro_use]
mod macros;

mod stage;
mod temporary_file;
pub use temporary_file::*;

pub mod utils;
pub use stage::*;

pub mod mro;
pub use log::LevelFilter;
/// For convenience
pub use mro::*;
pub mod prelude;

pub fn initialize(args: Vec<String>) -> Result<Metadata> {
    let mut md = Metadata::new(args);
    md.update_jobinfo()?;

    Ok(md)
}

#[cold]
fn write_errors(msg: &str, is_assert: bool) -> Result<()> {
    let mut err_file: File = unsafe { File::from_raw_fd(4) };

    // We want to aggressively avoid allocations here if we can, since one
    // common source of errors is running out of memory.
    let msg_alloc: String;
    let msg = if is_assert {
        msg_alloc = ["ASSERT:", msg].concat();
        msg_alloc.as_str()
    } else {
        msg
    };

    let _ = err_file.write_all(msg.as_bytes());

    // Avoid closing err_file
    let _ = err_file.into_raw_fd();
    Ok(())
}

// e.g. 2006-01-02 15:04:05.  Note that this is only crate-public, not fully
// public, because this is a bad date format that is used only to retain
// backwards compatibility.  For use cases where that is not a concern, use
// Rfc3339, which looks similar but won't cause strange bugs if not every
// compute node in your cluster is using the same time zone.
//
// We could use the proc macro, but then we'd need
// to compile the proc macro crate, which would slow down build times
// significantly for very little benefit in readability.
pub(crate) const DATE_FORMAT: &[FormatItem<'_>] = &[
    FormatItem::Component(Component::Year(Year::default())),
    Literal(b"-"),
    FormatItem::Component(Component::Month(Month::default())),
    Literal(b"-"),
    FormatItem::Component(Component::Day(Day::default())),
    Literal(b" "),
    FormatItem::Component(Component::Hour(Hour::default())),
    Literal(b":"),
    FormatItem::Component(Component::Minute(Minute::default())),
    Literal(b":"),
    FormatItem::Component(Component::Second(Second::default())),
];

fn setup_logging(log_file: File, level: LevelFilter) {
    let base_config = fern::Dispatch::new().level(level);

    let logger_config = fern::Dispatch::new()
        .format(|out, msg, record| {
            let time_str = OffsetDateTime::now_local()
                .unwrap_or_else(|_| OffsetDateTime::now_utc())
                .format(DATE_FORMAT)
                .unwrap();
            out.finish(format_args!("{time_str} [{}] {msg}", record.target()))
        })
        .chain(log_file)
        .chain(io::stdout());

    let cfg = base_config.chain(logger_config).apply();

    if let Err(e) = cfg {
        panic!("Failed to initialize global logger: {e}");
    }
}

/// Configure the Martian adapter for executing stage code
pub struct MartianAdapter<S> {
    stage_map: HashMap<String, Box<dyn RawMartianStage>, S>,
    log_level: LevelFilter,
    is_error_assert: Box<dyn (Fn(&Error) -> bool) + 'static>,
}

impl<S: std::hash::BuildHasher> MartianAdapter<S> {
    /// Build a new Martian adapter with the given registry of Martian stages
    /// Arguments:
    ///  - `stage_map`: names and implementations of the Martian stages that can be run by this binary.
    pub fn new(stage_map: HashMap<String, Box<dyn RawMartianStage>, S>) -> MartianAdapter<S> {
        MartianAdapter {
            stage_map,
            log_level: LevelFilter::Warn,
            is_error_assert: Box::new(|_| false),
        }
    }

    /// Set the minimum severity level of log messages that are emitted to the Martian
    /// _log file.
    pub fn log_level(self, log_level: LevelFilter) -> MartianAdapter<S> {
        MartianAdapter { log_level, ..self }
    }

    ///  Set `is_error_assert`, predicate determining whether to emit an error as an ASSERT
    ///  to Martian. ASSERT errors indicate an unrecoverable configuration error, and will
    ///  prevent the user from restarting the pipeline. The is_error_assert function should
    ///  use downcasting to match the error against a set of error types that should generate an assert.
    pub fn assert_if<F: 'static + Fn(&Error) -> bool>(self, predicate: F) -> MartianAdapter<S> {
        MartianAdapter {
            is_error_assert: Box::new(predicate),
            ..self
        }
    }

    /// Run the martian adapter using the given cmdline args
    /// provided by the martian runtime. The caller should call sys::exit() witih
    /// the returncode returned by this function.
    /// Arguments:
    ///  - `args`: vector of command line arguments, typically supplied by Martian runtime.
    #[must_use = "Martian stage binaries should call std::process::exit() on the return_code"]
    pub fn run(self, args: Vec<String>) -> i32 {
        self.run_get_error(args).0
    }

    /// Like `run()` but also return an error thrown by the stage (if any). May be useful
    /// for unit testing purposes.
    #[must_use = "Martian stage binaries should call std::process::exit() on the return_code"]
    pub fn run_get_error(self, args: Vec<String>) -> (i32, Option<Error>) {
        martian_entry_point(args, self.stage_map, self.log_level, self.is_error_assert)
    }
}

/// See docs on MartianAdapter methods for details.
fn martian_entry_point<S: std::hash::BuildHasher>(
    args: Vec<String>,
    stage_map: HashMap<String, Box<dyn RawMartianStage>, S>,
    level: LevelFilter,
    is_error_assert: Box<dyn Fn(&Error) -> bool>,
) -> (i32, Option<Error>) {
    info!("got args: {:?}", args);

    // turn on backtrace capture
    std::env::set_var("RUST_BACKTRACE", "1");

    // Hook rust logging up to Martian _log file
    let log_file: File = unsafe { File::from_raw_fd(3) };
    setup_logging(log_file, level);

    // setup Martian metadata
    // special handler for error in stage setup
    let mut md = match initialize(args).context("IO Error initializing stage") {
        Ok(m) => m,
        Err(e) => {
            let _ = write_errors(&format!("{e:?}"), false);
            return (1, Some(e));
        }
    };

    // Get the stage implementation
    let stage = stage_map.get(&md.stage_name).with_context(
        #[cold]
        || format!("Couldn't find requested Martian stage: {}", md.stage_name),
    );

    // special handler for non-existent stage
    let stage = match stage {
        Ok(s) => s,
        Err(e) => {
            let _ = write_errors(&format!("{e:?}"), false);
            return (1, Some(e));
        }
    };

    // will write to this from panic handler if needed.
    // panic handler has separate write code to avoid locking.
    let stackvars_path = md.make_path("stackvars");

    // Setup panic hook. If a stage panics, we'll shutdown cleanly to martian
    let p = panic::take_hook();
    panic::set_hook(Box::new(
        #[cold]
        move |info| {
            let backtrace = Backtrace::new();

            let msg = match info.payload().downcast_ref::<&'static str>() {
                Some(&s) => s,
                None => match info.payload().downcast_ref::<String>() {
                    Some(s) => (*s).as_str(),
                    None => "Box<Any>",
                },
            };

            let msg = match info.location() {
                Some(location) => format!(
                    "stage failed unexpectedly: '{}' {}:{}:\n{:?}",
                    msg,
                    location.file(),
                    location.line(),
                    backtrace
                ),
                None => format!("stage failed unexpectedly: '{msg}':\n{backtrace:?}"),
            };

            // write to _log
            error!("{}", msg);

            // write stack trace to to _stackvars.
            // this will just give up if any errors are encountere
            let bt_string = format!("{backtrace:?}");
            let _ = File::create(&stackvars_path).map(move |mut f| {
                let _ = f.write_all(bt_string.as_bytes());
            });

            // write to _errors
            let _ = write_errors(&msg, false);

            // call default panic handler (not sure if this is a good idea or not)
            p(info);
        },
    ));

    let result = if md.stage_type == "split" {
        stage.split(&mut md)
    } else if md.stage_type == "main" {
        stage.main(&mut md)
    } else if md.stage_type == "join" {
        stage.join(&mut md)
    } else {
        panic!("Unrecognized stage type");
    };

    match result {
        // exit code = 0
        Ok(()) => (0, None),

        // write message and stack trace, exit code = 1;
        Err(e) => {
            report_error(&mut md, &e, is_error_assert(&e));
            (1, Some(e))
        }
    }
}

#[cold]
fn report_error(md: &mut Metadata, e: &Error, is_assert: bool) {
    let bt = e.backtrace();
    let _ = md.stackvars(&bt.to_string());
    let _ = write_errors(&format!("{e:#}"), is_assert);
}

/// Return the environment variable CARGO_PKG_NAME or the current executable name.
fn get_generator_name() -> String {
    std::env::var("CARGO_PKG_NAME").unwrap_or_else(|_| current_executable())
}

/// Write MRO to filename or stdout.
pub fn martian_make_mro(
    header_comment: &str,
    filename: Option<impl AsRef<Path>>,
    rewrite: bool,
    mro_registry: Vec<StageMro>,
) -> Result<()> {
    if let Some(filename) = &filename {
        let filename = filename.as_ref();
        ensure!(
            !filename.is_dir(),
            "Path {} is a directory",
            filename.display()
        );
        ensure!(
            rewrite || !filename.exists(),
            "File {} exists. Use --rewrite to overwrite it.",
            filename.display()
        );
    }

    let mro = make_mro_string(header_comment, &mro_registry);
    match filename {
        Some(filename) => {
            let filename = filename.as_ref();
            File::create(filename)
                .with_context(|| filename.display().to_string())?
                .write_all(mro.as_bytes())
                .with_context(|| filename.display().to_string())?;
        }
        None => {
            print!("{mro}");
        }
    }
    Ok(())
}

pub fn make_mro_string(header_comment: &str, mro_registry: &[StageMro]) -> String {
    let mut filetype_header = FiletypeHeader::default();
    let mut struct_header = StructHeader::default();
    let mut mro_string = String::new();
    for stage_mro in mro_registry {
        filetype_header.add_stage(stage_mro);
        struct_header.add_stage(stage_mro);
        writeln!(&mut mro_string, "{stage_mro}").unwrap();
    }
    mro_string.pop();

    if header_comment.is_empty() {
        format!(
            "#
# Code generated by {}.  DO NOT EDIT.
#

{}{}{}",
            get_generator_name(),
            filetype_header,
            struct_header,
            mro_string
        )
    } else {
        assert!(
            header_comment
                .lines()
                .all(|line| line.trim_end().is_empty() || line.starts_with('#')),
            "All non-empty header lines must start with '#', but got\n{header_comment}"
        );
        format!(
            "{}
#
# Code generated by {}.  DO NOT EDIT.
#

{}{}{}",
            header_comment,
            get_generator_name(),
            filetype_header,
            struct_header,
            mro_string
        )
    }
}
