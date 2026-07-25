# Add an application health-status feature

Implement a small but complete health-status feature across the existing application layers.

## Required behavior

- Add a lightweight backend health endpoint using the repository's existing server framework and routing conventions.
- Return structured JSON containing at least an overall status and a server timestamp. Include a safe dependency check for the application's existing database/Supabase integration when that can be done without exposing credentials or private data.
- Add a compact React UI status indicator that calls the endpoint and presents loading, healthy, and unavailable states accessibly.
- Follow the application's existing styling, data-fetching, error-handling, and TypeScript conventions rather than introducing a second framework or state-management system.
- Add meaningful automated tests for the endpoint and UI behavior using the test tools already present in the repository.
- Document the endpoint briefly in the most appropriate existing developer document if the repository has one.

## Constraints

- Do not expose environment variables, secrets, database details, stack traces, or internal hostnames in the response.
- Do not add a new production dependency unless it is genuinely necessary.
- Do not alter unrelated behavior or perform broad refactoring.
- If this repository has no backend, implement the nearest architecture-appropriate equivalent and clearly explain that decision in the final report.

## Completion condition

The implementation, tests, type checks, and relevant build must pass, or the final report must identify a concrete pre-existing/environmental blocker with supporting command output.
