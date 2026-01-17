# Future Tasks Prompts

This document contains prompts for future documentation and onboarding tasks.

## Prompt 1: Documentation Hosting Feature

I need to design a documentation system for BiblioGenius, an open-source library management app with a Rust backend (Axum + SeaORM) and Flutter frontend.

**Requirements:**

1. **Dual Documentation Sites**
   - Rust backend API documentation (generated from rustdoc)
   - Flutter frontend documentation (generated from dartdoc)
   - Both hosted at bibliogenius.org (e.g., docs.bibliogenius.org/rust and docs.bibliogenius.org/flutter)

2. **Synchronized Generation**
   - Documentation should be auto-generated from code comments
   - Must stay in sync with the codebase (CI/CD integration)
   - Version-tagged docs (matching release versions)

3. **Technical Constraints**
   - Rust: Edition 2024, Axum 0.7, SeaORM 0.12
   - Flutter: SDK 3.x, Provider state management
   - Current hosting: likely static site on bibliogenius.org

**Deliverables needed:**

- Architecture design for the doc generation pipeline
- CI/CD workflow (GitHub Actions preferred)
- Hosting strategy (subdomain vs path-based routing)
- Cross-linking strategy between Rust and Flutter docs
- Recommended documentation standards/conventions for contributors

Please provide a detailed implementation plan with specific tooling recommendations.
These documentations would be ideally accessibles by agents so they can have more context before coding

---

## Prompt 2: Developer Contribution Tutorial

I need to create a step-by-step onboarding tutorial for developers who want to contribute to BiblioGenius, an open-source library management application.

**Project Stack:**

- Backend: Rust (Axum 0.7, SeaORM 0.12, SQLite)
- Frontend: Flutter 3.x (Provider, GoRouter, Dio)
- Communication: FFI for native platforms, HTTP for web/debug
- Structure: monorepo with `bibliogenius/` (Rust) and `bibliogenius-app/` (Flutter)

**Tutorial Should Cover:**

1. **Environment Setup**
   - Prerequisites (Rust toolchain, Flutter SDK, IDE setup)
   - Cloning and initial build
   - Running the app locally (FFI mode vs HTTP mode)

2. **Architecture Understanding**
   - How Rust and Flutter communicate
   - Key directories and their purposes
   - Database migrations and SeaORM entities

3. **Development Workflow**
   - Making changes to Rust backend
   - Making changes to Flutter frontend
   - Running tests (Rust: cargo test, Flutter: flutter test)
   - Code style and conventions (see CLAUDE.md for details)

4. **Contribution Process**
   - Git workflow (branching, commits)
   - PR guidelines
   - Code review expectations

5. **Common Tasks with Examples**
   - Adding a new API endpoint
   - Adding a new Flutter screen
   - Adding translations (mandatory EN + FR)
   - Integrating a new external data source

**Target Audience:** Intermediate developers familiar with either Rust or Flutter, but possibly not both.

**Deliverables:**

- Structured tutorial outline (can be multiple pages/sections)
- Concrete code examples for each common task
- Troubleshooting section for common setup issues
- Suggested format (markdown docs, interactive guide, video scripts?)

Please design this tutorial with progressive complexity, allowing contributors to start with simple tasks before tackling cross-stack features.

This tutorial would be ideally accessible by agents so they can have more context before coding

---

## Prompt 3: Notes & Highlights Feature with Module System

**Role**: Senior Flutter & Systems Architect specializing in Rust/Flutter integration.

**Context**: BiblioGenius is a personal library management app (Flutter + Rust backend). We are introducing a "Notes & Highlights" feature for books.

**Core Requirements:**

1. **Modular Architecture**
   - The "Notes" feature must be implemented as a distinct **Module** that can be enabled or disabled in the app settings.
   - This modularity should follow or establish a pattern for future optional features.

2. **Safety & Data Protection (Critical)**
   - **Disabling Logic**: Users *can* disable the module, but this action is destructive (hides/removes notes).
   - **Explicit Warning**: The disable flow MUST trigger a strict confirmation dialog: *"Disabling this module will delete all your notes. Are you sure?"*
   - **Mandatory Export Prompt**: Before the final deletion confirmation, the user MUST be informed that they can export their notes. The flow should offer an "Export Notes" button alongside the "Delete & Disable" button.

3. **Data Specifications**
   - **Entity**: Create a `Note` entity linked to a `Book`.
   - **Fields**: Text content, page number/location, creation date, and metadata to support future Kindle imports (e.g., `original_source`, `highlight_timestamp`, `location_range`).
   - **Storage**: Persisted via Rust backend (SeaORM/SQLite).

4. **Integration**
   - **UI**: Seamlessly integrate into `BookDetailsScreen` when enabled.
   - **Kindle Strategy**: Outline a plan for a parser (e.g., `My Clippings.txt`) to run locally.

**Deliverables Needed:**

- **Data Model Schema**: Rust structs and database migration plan.
- **Module Logic**: State management design for enabling/disabling modules.
- **Safety Flow Design**: UX description or pseudo-code for the Disable -> Export-Warning -> Confirm -> Delete flow.
- **Implementation Plan**: Step-by-step guide to build this feature.
