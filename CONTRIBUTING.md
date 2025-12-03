# Contributing to BiblioGenius

Thank you for your interest in contributing to BiblioGenius! We welcome contributions from everyone.

## How to Contribute

### Reporting Bugs

1. Check the [Issue Tracker](https://github.com/bibliogenius/bibliogenius/issues) to see if the bug has already been reported.
2. If not, open a new issue. Provide a clear title and description, including steps to reproduce the issue.

### Suggesting Enhancements

1. Open a new issue on GitHub.
2. Describe the enhancement and why it would be useful.

### Pull Requests

1. Fork the repository.
2. Create a new branch for your feature or fix: `git checkout -b feature/my-feature`.
3. Make your changes and commit them: `git commit -m "feat: add my feature"`.
    * We follow [Conventional Commits](https://www.conventionalcommits.org/).
4. Push to your branch: `git push origin feature/my-feature`.
5. Open a Pull Request against the `main` branch.

## Development Setup

### Prerequisites

* Rust (latest stable)
* Docker (optional, for DB)

### Running Locally

```bash
# 1. Install dependencies
cargo build

# 2. Run the server
cargo run
```

## Code Style

* Run `cargo fmt` before committing.
* Run `cargo clippy` to check for common mistakes.

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
