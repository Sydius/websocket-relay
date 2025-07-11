# Contributing to WebSocket Relay

Thank you for your interest in contributing to WebSocket Relay! This document
provides guidelines for contributing to the project.

## Development Setup

1. **Prerequisites**
   - Rust (stable, beta, or nightly)
   - Git

2. **Clone the repository**

   ```bash
   git clone https://github.com/Sydius/websocket-relay.git
   cd websocket-relay
   ```

3. **Build the project**

   ```bash
   cargo build
   ```

4. **Run tests**

   ```bash
   cargo test
   ```

## Code Standards

### Formatting and Linting

- **Formatting**: Use `cargo fmt` to format code
- **Linting**: Run `cargo clippy --all-features --all-targets` to check for
  issues
- **Required**: All clippy warnings must be addressed before submitting

### Code Quality

- Follow Rust best practices and idioms
- Write tests for new functionality

### Commit Messages

This project uses [Conventional Commits](https://www.conventionalcommits.org/).
Use these prefixes:

- `feat:` for new features
- `fix:` for bug fixes
- `docs:` for documentation changes
- `refactor:` for code refactoring
- `test:` for adding tests
- `chore:` for maintenance tasks

## Submitting Changes

1. **Fork the repository** and create a feature branch from `main`
2. **Make your changes** following the code standards above
3. **Test thoroughly** - ensure all tests pass and new functionality works
4. **Commit** using conventional commit format
5. **Push** to your fork and submit a pull request

## Pull Request Guidelines

- **Title**: Use conventional commit format
- **Description**: Clearly describe what the PR does and why
- **Testing**: Describe how you tested the changes
- **Breaking changes**: Clearly mark any breaking changes

## Security Considerations

This project handles network connections and potentially sensitive data:

- Be mindful of security implications in networking code
- Validate all input from external sources
- Follow secure coding practices for TLS/SSL handling
- Report security issues privately to the maintainers

## Getting Help

- Check existing issues before creating new ones
- Provide clear reproduction steps for bugs
- Include relevant configuration and environment details

## License

By contributing, you agree that your contributions will be licensed under the
MIT License.
