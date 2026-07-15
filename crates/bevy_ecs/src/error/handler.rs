use core::fmt::Display;

use crate::{
    change_detection::Tick,
    error::BevyError,
    prelude::Resource,
    system::Commands,
    world::{unsafe_world_cell::UnsafeWorldCell, CommandQueue, DeferredWorld, World},
};
use bevy_ecs::error::Severity;
use bevy_utils::prelude::DebugName;
use derive_more::derive::{Deref, DerefMut};

/// Context for a [`BevyError`] to aid in debugging.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ErrorContext {
    /// The error occurred in a system.
    System {
        /// The name of the system that failed.
        name: DebugName,
        /// The last tick that the system was run.
        last_run: Tick,
    },
    /// The error occurred in a run condition.
    RunCondition {
        /// The name of the run condition that failed.
        name: DebugName,
        /// The last tick that the run condition was evaluated.
        last_run: Tick,
        /// The system this run condition is attached to.
        system: DebugName,
        /// `true` if this run condition was on a set.
        on_set: bool,
    },
    /// The error occurred in a command.
    Command {
        /// The name of the command that failed.
        name: DebugName,
    },
    /// The error occurred in an observer.
    Observer {
        /// The name of the observer that failed.
        name: DebugName,
        /// The last tick that the observer was run.
        last_run: Tick,
    },
}

impl Display for ErrorContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::System { name, .. } => {
                write!(f, "System `{name}` failed")
            }
            Self::Command { name } => write!(f, "Command `{name}` failed"),
            Self::Observer { name, .. } => {
                write!(f, "Observer `{name}` failed")
            }
            Self::RunCondition {
                name,
                system,
                on_set,
                ..
            } => {
                write!(
                    f,
                    "Run condition `{name}` failed for{} system `{system}`",
                    if *on_set { " set containing" } else { "" }
                )
            }
        }
    }
}

impl ErrorContext {
    /// The name of the ECS construct that failed.
    pub fn name(&self) -> DebugName {
        match self {
            Self::System { name, .. }
            | Self::Command { name, .. }
            | Self::Observer { name, .. }
            | Self::RunCondition { name, .. } => name.clone(),
        }
    }

    /// A string representation of the kind of ECS construct that failed.
    ///
    /// This is a simpler helper used for logging.
    pub fn kind(&self) -> &str {
        match self {
            Self::System { .. } => "system",
            Self::Command { .. } => "command",
            Self::Observer { .. } => "observer",
            Self::RunCondition { .. } => "run condition",
        }
    }
}

macro_rules! inner {
    ($call:path, $e:ident, $c:ident) => {
        $call!(
            "Encountered an error in {} `{}`: {}",
            $c.kind(),
            $c.name(),
            $e
        );
    };
}

/// Defines how Bevy reacts to errors.
///
/// The handler can use [`Commands`] to send messages or perform cleanup work. It must return the
/// commands after queuing any work so the caller can retain ownership of the command queue.
///
/// When writing an error handler, if you want to throw a panic,
/// consider setting [`PANIC_ORIGINATES_FROM_ERROR_HANDLER`].
/// This lets the executor know that a panic doesn't need to be
/// converted back to a [`BevyError`] and passed to the [`FallbackErrorHandler`].
pub type ErrorHandler =
    for<'w, 's> fn(BevyError, ErrorContext, Commands<'w, 's>) -> Commands<'w, 's>;

/// Fallback error handler to call when an error is not handled otherwise.
/// Defaults to [`match_severity()`].
///
/// Called both for explicitly returned errors, and when a panic occurs.
///
/// When updated while a [`Schedule`] is running, it doesn't take effect for
/// that schedule until it's completed.
///
/// [`Schedule`]: crate::schedule::Schedule
#[derive(Resource, Deref, DerefMut, Copy, Clone)]
pub struct FallbackErrorHandler(pub ErrorHandler);

impl Default for FallbackErrorHandler {
    fn default() -> Self {
        Self(match_severity)
    }
}

/// A command queue used to defer work produced by an [`ErrorHandler`].
///
/// Error handlers can run while systems have borrowed the world, so their commands must be
/// collected separately and applied at the next deferred-command boundary.
pub(crate) struct ErrorHandlerCommandQueue(Option<CommandQueue>);

impl ErrorHandlerCommandQueue {
    pub(crate) const fn new() -> Self {
        Self(None)
    }

    /// Runs `handler` with commands that write to this queue.
    pub(crate) fn handle(
        &mut self,
        handler: ErrorHandler,
        error: BevyError,
        context: ErrorContext,
        world: UnsafeWorldCell,
    ) {
        let queue = self.0.get_or_insert_with(CommandQueue::default);
        let commands =
            Commands::new_from_entities(queue, world.entity_allocator(), world.entities());
        handler(error, context, commands);
    }

    /// Applies queued error-handler commands, if any.
    pub(crate) fn apply(&mut self, world: &mut World) {
        if let Some(queue) = self.0.as_mut()
            && !queue.is_empty()
        {
            queue.apply(world);
        }
    }

    /// Moves queued error-handler commands into the world's command queue.
    pub(crate) fn queue(&mut self, world: &mut DeferredWorld) {
        if let Some(queue) = self.0.as_mut()
            && !queue.is_empty()
        {
            world.commands().append(queue);
        }
    }
}

impl Default for ErrorHandlerCommandQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "std")]
std::thread_local! {
    /// When deliberately throwing a panic in your [`ErrorHandler`],
    /// set this to true to indicate to the executor that the panic
    /// should not be turned back into a [`BevyError`].
    pub static PANIC_ORIGINATES_FROM_ERROR_HANDLER: core::cell::Cell<bool>  = const {core::cell::Cell::new(false)};
}

/// Error handler that defers to an error's [`Severity`].
#[track_caller]
#[inline]
pub fn match_severity<'w, 's>(
    err: BevyError,
    ctx: ErrorContext,
    commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    match err.severity() {
        Severity::Ignore => ignore(err, ctx, commands),
        Severity::Trace => trace(err, ctx, commands),
        Severity::Debug => debug(err, ctx, commands),
        Severity::Info => info(err, ctx, commands),
        Severity::Warning => warn(err, ctx, commands),
        Severity::Error => error(err, ctx, commands),
        Severity::Panic => panic(err, ctx, commands),
    }
}

/// Error handler that panics with the system error.
#[track_caller]
#[inline]
pub fn panic<'w, 's>(
    error: BevyError,
    ctx: ErrorContext,
    _commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    #[cfg(feature = "std")]
    PANIC_ORIGINATES_FROM_ERROR_HANDLER.set(true);
    inner!(panic, error, ctx);
}

/// Error handler that logs the system error at the `error` level.
#[track_caller]
#[inline]
pub fn error<'w, 's>(
    error: BevyError,
    ctx: ErrorContext,
    commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    inner!(log::error, error, ctx);
    commands
}

/// Error handler that logs the system error at the `warn` level.
#[track_caller]
#[inline]
pub fn warn<'w, 's>(
    error: BevyError,
    ctx: ErrorContext,
    commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    inner!(log::warn, error, ctx);
    commands
}

/// Error handler that logs the system error at the `info` level.
#[track_caller]
#[inline]
pub fn info<'w, 's>(
    error: BevyError,
    ctx: ErrorContext,
    commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    inner!(log::info, error, ctx);
    commands
}

/// Error handler that logs the system error at the `debug` level.
#[track_caller]
#[inline]
pub fn debug<'w, 's>(
    error: BevyError,
    ctx: ErrorContext,
    commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    inner!(log::debug, error, ctx);
    commands
}

/// Error handler that logs the system error at the `trace` level.
#[track_caller]
#[inline]
pub fn trace<'w, 's>(
    error: BevyError,
    ctx: ErrorContext,
    commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    inner!(log::trace, error, ctx);
    commands
}

/// Error handler that ignores the system error.
#[track_caller]
#[inline]
pub fn ignore<'w, 's>(
    _: BevyError,
    _: ErrorContext,
    commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    commands
}
