# Readme for AI Agents

## Build/Lint/Test Commands

- Build: `cargo build`
- Build the CLI only: `cargo build -p battlesnake` (own crate in `cli/`, no server dependencies)
- Run: `cargo run`
- Check: `cargo check`
- Lint: `cargo clippy`
- Fix auto-correctable lints: `cargo clippy --fix` (always try this first before manually fixing lints)
- Format: `cargo fmt`
- Test: `cargo test`
- Test single: `cargo test <test_name>`
- Run database migrations: `sqlx migrate run`
- Create migration: `sqlx migrate add --source migrations <migration_name>`
- Recreate DB from scratch: `cargo sqlx db drop -y && cargo sqlx db create && cargo sqlx migrate run`

## Database Migration Naming Conventions

SQLx migrations should follow these naming conventions:

- **Up migrations**: Always use the `.up.sql` extension (e.g., `20250415155810_add_visibility_to_battlesnakes.up.sql`)
- **Down migrations**: Always use the `.down.sql` extension (e.g., `20250415155810_add_visibility_to_battlesnakes.down.sql`)

When creating migrations with `sqlx migrate add`, SQLx will create a file with the timestamp and name, but without the `.up.sql` extension. You should rename this file manually to follow the convention above, and create a corresponding `.down.sql` file for rollbacks.

## SQLx Configuration

SQLx uses offline query checking, which requires either a connection to the database or a query cache. When encountering SQL query errors like `set DATABASE_URL to use query macros online, or run cargo sqlx prepare to update the query cache`, use one of these solutions:

1. **Update the Query Cache:**

   ```
   DATABASE_URL="postgresql://localhost:5432/arena" cargo sqlx prepare --workspace
   ```

   This will generate `.sqlx` files in the project root, which should be checked into version control.

2. **Set DATABASE_URL Environment Variable:**
   When running commands that use SQLx macros, ensure the DATABASE_URL is set:
   ```
   export DATABASE_URL="postgresql://localhost:5432/arena"
   cargo build
   ```

For more information, see [SQLx documentation on offline mode](https://docs.rs/sqlx/latest/sqlx/macro.query.html#offline-mode-requires-the-offline-feature).

## Background Jobs

For background work, don't use `tokio::spawn`. Instead, use the cja job system:

1. Create a new job struct that implements the `Job` trait (must also implement Default):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MyJob {
    // Store MINIMAL data needed to identify work - prefer database IDs
    // Don't store data that can be looked up from the database
    pub entity_id: i32, // Example: just store the ID of what to process
}

#[async_trait::async_trait]
impl Job<AppState> for MyJob {
    const NAME: &'static str = "MyJob";

    async fn run(&self, app_state: AppState) -> cja::Result<()> {
        // Look up latest data from the database
        let entity = get_entity_by_id(&app_state.db, self.entity_id).await?;

        // Process using the latest data from the database
        Ok(())
    }
}
```

2. Add the job to the job registry in `src/jobs.rs`:

```rust
// Add to the macro call
cja::impl_job_registry!(AppState, NoopJob, UpdateProfileHandleJob, MyJob);
```

4. To enqueue a job for background processing:

```rust
MyJob { some_field: "value".to_string() }
    .enqueue(&app_state)
    .await?;
```

Jobs are processed in the background and will retry on failure.

## Code Style Guidelines

- Error handling: Use `cja::Result`, propagate with `?` operator
- Naming: snake_case for variables/functions, CamelCase for types, UPPERCASE for constants
- Imports: Group by crate, organize logically, prefer explicit imports
- Async: Use async/await throughout with Tokio runtime
- State: Pass AppState as context throughout application
- Modules: Organize by functionality (routes, jobs, etc.)
- Documentation: Document public interfaces
- Error messages: Be descriptive and actionable
- Error propagation: Use `?` operator, avoid unwrap/expect in production code
- Tracing: Use tracing macros for observability (info, debug, etc.)

## Error Handling Guidelines

We use `color-eyre` for error handling throughout the codebase. Follow these best practices:

1. Always prefer using the `?` operator with `wrap_err` or `wrap_err_with` instead of match statements for error handling:

```rust
// GOOD
let data = some_function().wrap_err("Failed to get data")?;

// AVOID when only adding context
let data = match some_function() {
    Ok(d) => d,
    Err(e) => return Err(eyre!("Failed to get data: {}", e)),
};
```

2. Use `wrap_err` for static error messages:

```rust
// GOOD
.wrap_err("Failed to decode file")?;

// AVOID for static strings
.wrap_err_with(|| "Failed to decode file")?;
```

3. Use `wrap_err_with` only when you need to generate dynamic error messages:

```rust
// GOOD - Dynamic content in error message
.wrap_err_with(|| format!("Failed to process file: {}", file_path))?;

// GOOD - Expensive computation only done if there's an error
.wrap_err_with(|| {
    let details = compute_error_details();
    format!("Failed with details: {}", details)
})?;
```

4. Always ensure the `Context` trait is imported:

```rust
use color_eyre::eyre::Context as _;
```

5. For web handlers, use the `ServerResult` type alias to handle errors consistently:

```rust
// Handler function signature pattern
async fn my_handler(
    State(state): State<AppState>,
    // ... other parameters
) -> ServerResult<impl IntoResponse, StatusCode> {
    // Function logic
    let result = some_operation().await
        .wrap_err("Failed to perform operation")?;

    Ok(result.into_response())
}
```

For handlers that need to return redirects on error:

```rust
async fn profile_handler(
    // ... parameters
) -> ServerResult<impl IntoResponse, Redirect> {
    let user_data = get_user_data().await
        .wrap_err("Failed to get user data")
        .with_redirect(Redirect::to("/login"))?;

    Ok(render_profile(user_data).into_response())
}
```

6. Use the following traits for converting errors to appropriate responses:

```rust
// For StatusCode responses
.with_status(StatusCode::BAD_REQUEST)?

// For Redirect responses
.with_redirect(Redirect::to("/error-page"))?
```

7. Don't use `.with_status(StatusCode::INTERNAL_SERVER_ERROR)` since internal server error (500) is already the default error status:

```rust
// GOOD - Let 500 be the default error response
.wrap_err("Failed to connect to service")?

// AVOID - Redundant status code specification
.wrap_err("Failed to connect to service")
.with_status(StatusCode::INTERNAL_SERVER_ERROR)?
```

Only specify error status codes that differ from the default 500, such as 400, 401, 403, 404, etc.

Important: Always import the necessary types and traits:

```rust
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use crate::errors::{ServerResult, ServerError, WithStatus, WithRedirect};
use color_eyre::eyre::{eyre, WrapErr};
```

The `ServerResult` type alias is defined as:

```rust
// Type alias for server handler results
pub type ServerResult<S, F> = Result<S, ServerError<F>>;

// ServerError wraps an eyre::Report with a response type
pub struct ServerError<R: IntoResponse>(pub(crate) cja::color_eyre::Report, pub(crate) R);
```

This pattern allows us to:

1. Include detailed error information (via Report)
2. Specify exactly what kind of response should be returned on error
3. Maintain type safety throughout our error handling

Remember the difference between `wrap_err` and `with_status`/`with_redirect`:

- `wrap_err` adds context to the error for debugging and logging
- `with_status`/`with_redirect` converts the error into an appropriate HTTP response
- Typically use them together: `operation().wrap_err("context").with_status(StatusCode::BAD_REQUEST)?`

8. Database queries: **Always** use the `sqlx::query!` and `sqlx::query_as!` macros instead of the non-macro versions. These macros provide compile-time SQL validation and type-checking, preventing runtime SQL errors.

## Avoiding panic in Rust

Never use `unwrap()` or `expect()` in production code as they can cause panics. Instead:

1. Use proper error handling with the `?` operator:

   ```rust
   // GOOD
   let value = some_fallible_operation().wrap_err("Operation failed")?;

   // BAD
   let value = some_fallible_operation().unwrap();
   ```

2. For Option types, use pattern matching or combinators:

   ```rust
   // GOOD
   let value = match optional_value {
       Some(v) => v,
       None => return Err(eyre!("Value not found"))?
   };

   // Or using combinators
   let value = optional_value.ok_or_else(|| eyre!("Value not found"))?;

   // BAD
   let value = optional_value.unwrap();
   ```

3. Use fallback values when appropriate:

   ```rust
   // GOOD - Using unwrap_or
   let value = optional_value.unwrap_or_default();
   let value = optional_value.unwrap_or(fallback_value);

   // GOOD - Using unwrap_or_else for computed fallbacks
   let value = optional_value.unwrap_or_else(|| compute_fallback());
   ```

4. In web handlers, return appropriate HTTP status codes instead of panicking:

   ```rust
   // GOOD
   if !is_valid {
       return Err(ServerError(eyre!("Invalid request"), StatusCode::BAD_REQUEST));
   }

   // BAD
   assert!(is_valid, "Request must be valid");
   ```
