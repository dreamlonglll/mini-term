# Repository Guidelines

## Project Structure & Module Organization

- `src/` contains the React 19 + TypeScript frontend. UI components live in `src/components/`, reusable hooks in `src/hooks/`, translations in `src/i18n/`, and shared helpers in `src/utils/`.
- `src-tauri/` contains the Tauri v2 Rust backend. Commands and application services are in `src-tauri/src/`; shared crates are under `mt-core/`, `mt-ssh/`, and `mt-sidecars/`.
- `tests/` contains Node test-runner regression tests (`*.test.cjs`).
- `relay-server/` hosts the Rust relay service and shared protocol crate; `mobile/` contains the companion PWA.
- `scripts/` contains sidecar and ConPTY staging utilities.

## Build, Test, and Development Commands

- `npm install` installs frontend and Tauri CLI dependencies. Use Node 20.19+ or 22.12+ and Rust 1.85+.
- `npm run tauri dev` starts the complete desktop development environment.
- `npm run build` type-checks and bundles the frontend.
- `npm run tauri build` stages sidecars and creates a production desktop package.
- `node --test tests/*.test.cjs` runs all Node regression tests.
- `npm run test:conpty` runs ConPTY packaging tests plus the related Rust unit tests.
- `cargo test --manifest-path src-tauri/Cargo.toml` runs backend tests.

## Coding Style & Naming Conventions

Use two-space indentation, single quotes, and semicolons in TypeScript/TSX. Name React components and files in `PascalCase`, hooks with `useCamelCase`, and utilities in `camelCase`. Keep shared types in `src/types.ts` or near their owning module. Reuse CSS variables from `src/styles.css` instead of hard-coded theme colors.

Rust code follows `rustfmt`: use `snake_case` for modules and functions and `PascalCase` for types. Run `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` before submitting Rust changes. No repository-wide ESLint or Prettier configuration is currently defined.

## Testing Guidelines

Add focused regression tests for behavior changes. Name Node tests after the affected feature, such as `tests/layoutRestore.test.cjs`. Keep Rust unit tests close to the implementation where practical. There is no enforced coverage percentage; prioritize critical terminal, filesystem, SSH, and persistence paths.

## Commit & Pull Request Guidelines

Follow the existing Conventional Commit style: `fix:`, `feat:`, `docs:`, `refactor:`, `chore:`, or `release:` followed by a concise description. Keep commits scoped and avoid including generated or temporary files.

Pull requests should explain the user-visible change, list verification commands, link relevant issues, and include screenshots or recordings for UI changes. Call out platform-specific behavior, permission changes, or configuration migrations explicitly.

## Security & Configuration Tips

Never commit SSH keys, credentials, relay secrets, or local configuration. Review changes to `src-tauri/capabilities/default.json` carefully and document why any new Tauri permission is required.
