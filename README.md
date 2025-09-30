# AeroSync - Cross-Platform File Transfer Engine

A high-performance, cross-platform batch file transfer engine built in Rust with support for multiple protocols and real-time monitoring.

## Features

- **Cross-Platform Support**: Windows, macOS, iOS, and Android
- **Multiple Protocols**: QUIC and HTTP with extensible architecture
- **File Receiver Server**: Act as a destination for file uploads from network clients
- **Real-Time Monitoring**: Transfer speeds, progress tracking, and statistics
- **Error Handling**: Comprehensive error handling for file I/O, network, and system issues
- **User Interaction**: File/folder selection and transfer cancellation
- **Resumable Transfers**: Support for pausing and resuming transfers
- **Batch Operations**: Handle multiple files and folders efficiently
- **Configurable Server**: Set custom receive directories, ports, and file size limits

## Architecture

The project is structured as a Rust workspace with the following crates:

- **aerosync-core**: Core transfer engine, file receiver server, progress monitoring, and error handling
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

**Client Features:**
- **📁 File Selection**: Native file and folder dialogs using `rfd` with support for single and multiple file selection
- **🎮 Transfer Controls**: Start, pause, and cancel transfers with intuitive buttons
- **📊 Real-time Progress**: Live progress bars and transfer statistics
- **⚙️ Settings Panel**: Configurable transfer parameters (concurrent transfers, chunk size, timeouts, protocol selection)
- **🎨 Modern UI**: Clean, responsive interface with emojis and color coding
- **📈 Live Statistics**: Real-time display of transfer speeds, completion rates, and active transfers

**✅ Server Features (NEW):**
- **🖥️ File Receiver Server**: Built-in server to receive files from network clients
- **📡 Server Status**: Real-time server status monitoring and control
- **📍 Network URLs**: Display HTTP and QUIC server URLs for client connections
- **📂 Directory Configuration**: Set custom receive directory with folder browser
- **⚙️ Server Settings**: Configure ports, file size limits, and protocol options
- **📥 Received Files**: Live display of received files with timestamps and sizes
- **🔄 Server Controls**: Start/stop server with one-click interface

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

## Server Usage Example

### Starting the File Receiver Server

1. **Launch AeroSync with GUI:**
   ```bash
   cargo run --features egui
   ```

2. **Open Server Panel:**
   - Click the "🖥 Server" button in the main interface
   - Configure server settings (ports, receive directory, file limits)
   - Click "▶ Start Server" to begin accepting files

3. **Server URLs:**  
   The server will display URLs like:
   - HTTP: `http://localhost:8080/upload`
   - QUIC: `quic://localhost:4433`

### Uploading Files to Server

**Using curl (HTTP):**
```bash
curl -X POST -F "file=@myfile.txt" http://localhost:8080/upload
```

**Using AeroSync Client:**
1. Enter the server URL as destination: `http://localhost:8080/upload`
2. Select files or folders to transfer
3. Click "▶ Start Transfer"

### Server Configuration Options

- **HTTP Port**: Default 8080 (configurable)
- **QUIC Port**: Default 4433 (configurable)  
- **Receive Directory**: Custom folder for received files
- **Max File Size**: Configurable limit (default 1GB)
- **Allow Overwrite**: Option to overwrite existing files
- **Protocol Selection**: Enable/disable HTTP or QUIC individually

## Roadmap

- [x] ✅ egui desktop interface implementation
- [x] ✅ File receiver server (HTTP + QUIC)
- [x] ✅ Server configuration and management UI
- [ ] Complete QUIC protocol implementation (basic structure done)
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