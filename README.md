# AeroSync - Cross-Platform File Transfer Engine

A high-performance, cross-platform batch file transfer engine built in Rust with support for multiple protocols and real-time monitoring.

## Features

- **Cross-Platform Support**: Windows, macOS, iOS, and Android
- **Multiple Protocols**: QUIC and HTTP with extensible architecture
- **Real-Time Monitoring**: Transfer speeds, progress tracking, and statistics
- **Error Handling**: Comprehensive error handling for file I/O, network, and system issues
- **User Interaction**: File/folder selection and transfer cancellation
- **Resumable Transfers**: Support for pausing and resuming transfers
- **Batch Operations**: Handle multiple files and folders efficiently

## Architecture

The project is structured as a Rust workspace with the following crates:

- **aerosync-core**: Core transfer engine, progress monitoring, and error handling
- **aerosync-protocols**: Protocol implementations (QUIC, HTTP)
- **aerosync-ui**: Cross-platform UI implementations (CLI, egui, Tauri)

## Quick Start

### Prerequisites

- Rust 1.70+ 
- Cargo

### Building

```bash
# Clone the repository
git clone <repository-url>
cd AeroSync

# Build the project
cargo build --release

# Run the CLI application
cargo run --bin aerosync
```

### Running with Different UI Backends

```bash
# CLI interface (default)
cargo run

# egui desktop interface (✅ IMPLEMENTED)
cargo run --features egui

# Tauri web-based interface (structure ready)
cargo run --features tauri
```

## Configuration

The transfer engine can be configured with:

- Maximum concurrent transfers
- Chunk size for file transfers
- Retry attempts for failed transfers
- Timeout settings
- Protocol selection (QUIC/HTTP)

## Protocol Support

### QUIC
- High-performance UDP-based protocol
- Built-in encryption and multiplexing
- Ideal for high-latency networks

### HTTP
- Standard HTTP/HTTPS support
- Range request support for resumable transfers
- Wide compatibility

## Development

### Project Structure

```
AeroSync/
├── aerosync-core/          # Core transfer engine
│   ├── src/
│   │   ├── error.rs        # Error types and handling
│   │   ├── progress.rs     # Progress monitoring
│   │   ├── transfer.rs     # Transfer engine
│   │   ├── file_manager.rs # File operations
│   │   └── lib.rs
│   └── Cargo.toml
├── aerosync-protocols/     # Protocol implementations
│   ├── src/
│   │   ├── traits.rs       # Protocol traits
│   │   ├── http.rs         # HTTP implementation
│   │   ├── quic.rs         # QUIC implementation
│   │   └── lib.rs
│   └── Cargo.toml
├── aerosync-ui/           # UI implementations
│   ├── src/
│   │   ├── cli.rs         # Command-line interface
│   │   ├── egui_app.rs    # egui desktop interface
│   │   ├── tauri_app.rs   # Tauri web interface
│   │   ├── events.rs      # Event system
│   │   └── lib.rs
│   └── Cargo.toml
├── src/
│   └── main.rs            # Main binary
├── Cargo.toml             # Workspace configuration
└── README.md
```

### Building for Different Platforms

```bash
# Windows
cargo build --target x86_64-pc-windows-gnu

# macOS
cargo build --target x86_64-apple-darwin

# Linux
cargo build --target x86_64-unknown-linux-gnu
```

### Testing

```bash
# Run all tests
cargo test

# Run tests for specific crate
cargo test -p aerosync-core
```

## UI Implementations

### ✅ egui Desktop Interface (COMPLETED)
The egui desktop interface provides a modern, cross-platform GUI with the following features:

- **📁 File Selection**: Native file and folder dialogs using `rfd`
- **🎮 Transfer Controls**: Start, pause, and cancel transfers with intuitive buttons
- **📊 Real-time Progress**: Live progress bars and transfer statistics
- **⚙️ Settings Panel**: Configurable transfer parameters (concurrent transfers, chunk size, timeouts, protocol selection)
- **🎨 Modern UI**: Clean, responsive interface with emojis and color coding
- **📈 Live Statistics**: Real-time display of transfer speeds, completion rates, and active transfers

**Usage:**
```bash
cargo run --features egui
```

### CLI Interface
Full-featured command-line interface for headless environments and automation.

**Usage:**
```bash
cargo run
```

## Roadmap

- [x] ✅ egui desktop interface implementation
- [ ] Complete QUIC protocol implementation
- [ ] Add file integrity verification (checksums)
- [ ] Implement bandwidth throttling
- [ ] Add cloud storage provider integrations
- [ ] Mobile platform support (iOS/Android)
- [ ] Web interface with Tauri
- [ ] Plugin system for custom protocols
- [ ] Distributed transfer coordination

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Add tests for new functionality
5. Submit a pull request

## License

[License information to be added]

## Security

This project implements defensive security practices:
- Input validation and sanitization
- Secure file handling
- Network security best practices
- Error handling without information leakage

For security issues, please contact [security contact information].