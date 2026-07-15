#[cfg(feature = "std")]
use core::panic::AssertUnwindSafe;
#[cfg(feature = "std")]
use std::backtrace::Backtrace;

use fixedbitset::FixedBitSet;

#[cfg(feature = "trace")]
use alloc::string::ToString as _;
#[cfg(feature = "trace")]
use tracing::info_span;

#[cfg(feature = "std")]
use crate::{
    error::{BevyError, Severity, PANIC_ORIGINATES_FROM_ERROR_HANDLER},
    system::BoxedSystem,
};
use crate::{
    error::{ErrorContext, ErrorHandler, ErrorHandlerCommandQueue},
    schedule::{
        is_apply_deferred, BoxedCondition, ConditionWithAccess, SystemExecutor, SystemSchedule,
    },
    system::{RunSystemError, ScheduleSystem},
    world::World,
};

#[cfg(feature = "hotpatching")]
use crate::{change_detection::DetectChanges, HotPatchChanges};

use super::{__rust_begin_short_backtrace, apply_deferred_conditions};

/// Runs the schedule using a single thread.
///
/// Useful if you're dealing with a single-threaded environment, saving your threads for
/// other things, or just trying minimize overhead.
#[derive(Default)]
pub struct SingleThreadedExecutor {
    /// System sets whose conditions have been evaluated.
    evaluated_sets: FixedBitSet,
    /// Systems that have run or been skipped.
    completed_systems: FixedBitSet,
    /// Systems that have run but have not had their buffers applied.
    unapplied_systems: FixedBitSet,
    /// Setting when true applies deferred system buffers after all systems have run
    apply_final_deferred: bool,
    /// Commands queued by the fallback error handler.
    error_handler_commands: ErrorHandlerCommandQueue,
}

impl SystemExecutor for SingleThreadedExecutor {
    fn init(&mut self, schedule: &SystemSchedule) {
        // pre-allocate space
        let sys_count = schedule.system_ids.len();
        let set_count = schedule.set_ids.len();
        self.evaluated_sets = FixedBitSet::with_capacity(set_count);
        self.completed_systems = FixedBitSet::with_capacity(sys_count);
        self.unapplied_systems = FixedBitSet::with_capacity(sys_count);
    }

    fn run(
        &mut self,
        schedule: &mut SystemSchedule,
        world: &mut World,
        _skip_systems: Option<&FixedBitSet>,
        error_handler: ErrorHandler,
    ) {
        let error_handler_commands = &mut self.error_handler_commands;

        // If stepping is enabled, make sure we skip those systems that should
        // not be run.
        #[cfg(feature = "bevy_debug_stepping")]
        if let Some(skipped_systems) = _skip_systems {
            // mark skipped systems as completed
            self.completed_systems |= skipped_systems;
        }

        #[cfg(feature = "hotpatching")]
        let hotpatch_tick = world
            .get_resource_ref::<HotPatchChanges>()
            .map(|r| r.last_changed())
            .unwrap_or_default();

        for system_index in 0..schedule.systems.len() {
            let system = &mut schedule.systems[system_index].system;

            #[cfg(feature = "trace")]
            let name = system.name();
            #[cfg(feature = "trace")]
            let should_run_span = info_span!("check_conditions", name = name.to_string()).entered();

            let mut should_run = !self.completed_systems.contains(system_index);
            for set_idx in schedule.sets_with_conditions_of_systems[system_index].ones() {
                if self.evaluated_sets.contains(set_idx) {
                    continue;
                }

                // evaluate system set's conditions
                let set_conditions_met = evaluate_and_fold_conditions(
                    &mut schedule.set_conditions[set_idx],
                    world,
                    error_handler,
                    error_handler_commands,
                    system,
                    true,
                );

                if !set_conditions_met {
                    self.completed_systems
                        .union_with(&schedule.systems_in_sets_with_conditions[set_idx]);
                }

                should_run &= set_conditions_met;
                self.evaluated_sets.insert(set_idx);
            }

            // evaluate system's conditions
            let system_conditions_met = evaluate_and_fold_conditions(
                &mut schedule.system_conditions[system_index],
                world,
                error_handler,
                error_handler_commands,
                system,
                false,
            );

            should_run &= system_conditions_met;

            #[cfg(feature = "trace")]
            should_run_span.exit();

            #[cfg(feature = "hotpatching")]
            if hotpatch_tick.is_newer_than(system.get_last_run(), world.change_tick()) {
                system.refresh_hotpatch();
            }

            // system has either been skipped or will run
            self.completed_systems.insert(system_index);

            if !should_run {
                continue;
            }

            if is_apply_deferred(&**system) {
                Self::apply_deferred(
                    &mut self.unapplied_systems,
                    schedule,
                    world,
                    error_handler,
                    error_handler_commands,
                );
                continue;
            }

            let f = |system: &mut _, world: &mut World, commands: &mut ErrorHandlerCommandQueue| {
                if let Err(RunSystemError::Failed(err)) =
                    __rust_begin_short_backtrace::run_without_applying_deferred(system, world)
                {
                    commands.handle(
                        error_handler,
                        err,
                        ErrorContext::System {
                            name: system.name(),
                            last_run: system.get_last_run(),
                        },
                        world.as_unsafe_world_cell(),
                    );
                }
            };

            #[cfg(feature = "std")]
            {
                handle_unwind(
                    f,
                    system,
                    world,
                    error_handler,
                    error_handler_commands,
                    "System panicked",
                );
            }

            #[cfg(not(feature = "std"))]
            {
                let f = f;
                (f)(system, world, error_handler_commands);
            }

            self.unapplied_systems.insert(system_index);
        }

        if self.apply_final_deferred {
            Self::apply_deferred(
                &mut self.unapplied_systems,
                schedule,
                world,
                error_handler,
                error_handler_commands,
            );
        }
        self.evaluated_sets.clear();
        self.completed_systems.clear();
    }

    fn set_apply_final_deferred(&mut self, apply_final_deferred: bool) {
        self.apply_final_deferred = apply_final_deferred;
    }
}

impl SingleThreadedExecutor {
    /// Creates a new single-threaded executor for use in a [`Schedule`].
    ///
    /// [`Schedule`]: crate::schedule::Schedule
    pub const fn new() -> Self {
        Self {
            evaluated_sets: FixedBitSet::new(),
            completed_systems: FixedBitSet::new(),
            unapplied_systems: FixedBitSet::new(),
            apply_final_deferred: true,
            error_handler_commands: ErrorHandlerCommandQueue::new(),
        }
    }

    fn apply_deferred(
        unapplied_systems: &mut FixedBitSet,
        schedule: &mut SystemSchedule,
        world: &mut World,
        error_handler: ErrorHandler,
        error_handler_commands: &mut ErrorHandlerCommandQueue,
    ) {
        for system_index in unapplied_systems.ones() {
            let system = &mut schedule.systems[system_index].system;
            #[cfg(not(feature = "std"))]
            {
                system.apply_deferred(world);
                let _ = error_handler;
            }

            #[cfg(feature = "std")]
            {
                handle_unwind(
                    |system, world, _| system.apply_deferred(world),
                    system,
                    world,
                    error_handler,
                    error_handler_commands,
                    "Encountered a panic while applying system buffers",
                );
            }
        }

        unapplied_systems.clear();
        apply_deferred_conditions(
            &mut schedule.system_conditions,
            &mut schedule.set_conditions,
            world,
        );
        error_handler_commands.apply(world);
    }
}

fn evaluate_and_fold_conditions(
    conditions: &mut [ConditionWithAccess],
    world: &mut World,
    error_handler: ErrorHandler,
    error_handler_commands: &mut ErrorHandlerCommandQueue,
    for_system: &ScheduleSystem,
    on_set: bool,
) -> bool {
    #[cfg(feature = "hotpatching")]
    let hotpatch_tick = world
        .get_resource_ref::<HotPatchChanges>()
        .map(|r| r.last_changed())
        .unwrap_or_default();

    #[expect(
        clippy::unnecessary_fold,
        reason = "Short-circuiting here would prevent conditions from mutating their own state as needed."
    )]
    conditions
        .iter_mut()
        .map(|ConditionWithAccess { condition, .. }| {
            #[cfg(feature = "hotpatching")]
            if hotpatch_tick.is_newer_than(condition.get_last_run(), world.change_tick()) {
                condition.refresh_hotpatch();
            }
            let f = |condition: &mut BoxedCondition,
                     world: &mut World,
                     commands: &mut ErrorHandlerCommandQueue| {
                __rust_begin_short_backtrace::readonly_run(&mut **condition, world).unwrap_or_else(
                    |err| {
                        if let RunSystemError::Failed(err) = err {
                            commands.handle(
                                error_handler,
                                err,
                                ErrorContext::RunCondition {
                                    name: condition.name(),
                                    last_run: condition.get_last_run(),
                                    system: for_system.name(),
                                    on_set,
                                },
                                world.as_unsafe_world_cell(),
                            );
                        };
                        false
                    },
                )
            };
            #[cfg(not(feature = "std"))]
            let result = {
                let f = f;
                f(condition, world, error_handler_commands)
            };
            #[cfg(feature = "std")]
            let result = handle_unwind_in_run_condition(
                f,
                condition,
                world,
                for_system,
                on_set,
                error_handler,
                error_handler_commands,
            );
            result
        })
        .fold(true, |acc, res| acc && res)
}

/// Handle a potential panic by invoking the error handler
#[cfg(feature = "std")]
fn handle_unwind(
    f: impl FnOnce(&mut BoxedSystem, &mut World, &mut ErrorHandlerCommandQueue),
    system: &mut BoxedSystem,
    world: &mut World,
    error_handler: ErrorHandler,
    error_handler_commands: &mut ErrorHandlerCommandQueue,
    error_message: &str,
) {
    PANIC_ORIGINATES_FROM_ERROR_HANDLER.set(false);
    let potential_unwind = std::panic::catch_unwind(AssertUnwindSafe(|| {
        f(system, world, error_handler_commands);
    }));
    let panic_originates_from_error_handler = PANIC_ORIGINATES_FROM_ERROR_HANDLER.replace(false);
    if let Err(payload) = potential_unwind {
        if panic_originates_from_error_handler {
            std::panic::resume_unwind(payload);
        }

        let err =
            BevyError::new_with_backtrace(Severity::Panic, error_message, Backtrace::disabled());
        __rust_begin_short_backtrace::error_handler(
            error_handler,
            err,
            ErrorContext::System {
                name: system.name(),
                last_run: system.get_last_run(),
            },
            error_handler_commands,
            world.as_unsafe_world_cell(),
        );
    }
}

/// Handle a potential panic by invoking the error handler
#[cfg(feature = "std")]
fn handle_unwind_in_run_condition(
    f: impl FnOnce(&mut BoxedCondition, &mut World, &mut ErrorHandlerCommandQueue) -> bool,
    condition: &mut BoxedCondition,
    world: &mut World,
    for_system: &ScheduleSystem,
    on_set: bool,
    error_handler: ErrorHandler,
    error_handler_commands: &mut ErrorHandlerCommandQueue,
) -> bool {
    PANIC_ORIGINATES_FROM_ERROR_HANDLER.set(false);
    let potential_unwind = std::panic::catch_unwind(AssertUnwindSafe(|| {
        f(condition, world, error_handler_commands)
    }));
    let panic_originates_from_error_handler = PANIC_ORIGINATES_FROM_ERROR_HANDLER.replace(false);
    match potential_unwind {
        Ok(r) => r,
        Err(payload) => {
            if panic_originates_from_error_handler {
                std::panic::resume_unwind(payload);
            }

            let err = BevyError::new_with_backtrace(
                Severity::Panic,
                "Encountered panic",
                Backtrace::disabled(),
            );
            __rust_begin_short_backtrace::error_handler(
                error_handler,
                err,
                ErrorContext::RunCondition {
                    name: condition.name(),
                    last_run: condition.get_last_run(),
                    system: for_system.name(),
                    on_set,
                },
                error_handler_commands,
                world.as_unsafe_world_cell(),
            );
            false
        }
    }
}
