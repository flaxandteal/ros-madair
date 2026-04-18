# Contributing to Rós Madair

We welcome contributions to Rós Madair.

## Contribution License

**Contributions must be MIT-licensed** to enable later license strategy changes.

By submitting a pull request, you agree that your contribution is licensed under
the [MIT License](https://opensource.org/licenses/MIT), and you grant the
project maintainers the right to distribute your contribution under the
project's current license (AGPL-3.0-or-later) or any future license chosen by
the project.

## Getting Started

1. Fork the repository
2. Clone your fork alongside the [alizarin](https://github.com/flaxandteal/alizarin) repository
3. Create a feature branch from `main`
4. Make your changes
5. Run the test suite: `cargo test --workspace`
6. Run clippy: `cargo clippy --workspace`
7. Submit a pull request

## Code Style

- Follow existing Rust conventions in the codebase
- All public items should have doc comments
- Keep `cargo clippy --workspace` warning-free
- Add tests for new functionality

## Reporting Issues

Open an issue on the GitHub repository with:

- A clear description of the problem or suggestion
- Steps to reproduce (for bugs)
- Expected vs actual behaviour
