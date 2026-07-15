---
title: "Error handlers now receive `Commands`"
pull_requests: []
---

Error handlers can now send messages and queue cleanup work using `Commands`.
As a result, the `ErrorHandler` signature has changed from
`fn(BevyError, ErrorContext)` to a function that accepts and returns `Commands`:

```rust
fn handle_error<'w, 's>(
    error: BevyError,
    context: ErrorContext,
    mut commands: Commands<'w, 's>,
) -> Commands<'w, 's> {
    // Log the error, send a message, or queue cleanup work.
    commands.write_message(AppExit::error());
    commands
}
```

The `Commands` value must be returned even when the handler does not queue any work.
This change also applies to the handlers passed to `Commands::queue_handled` and
`EntityCommands::queue_handled`.
Custom `SystemExecutor` implementations should use the `ErrorHandler` alias for their
`run` method's handler parameter.
