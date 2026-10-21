# `std::io`

Status: experimental

Stream-oriented I/O abstractions and process standard streams.

## Items

| Item | Signature | Description |
|---|---|---|
| `Reader` | `trait Reader` | Pull-style byte source. |
| `Writer` | `trait Writer` | Push-style byte sink. |
| `BufReader` | `type BufReader` | Buffered wrapper around any `Reader`. |
| `BufWriter` | `type BufWriter` | Buffered wrapper around any `Writer`. |
| `stdin` | `fn stdin() -> io::Reader` | Returns a handle to the process's standard input stream. Use read_line(&mut String) for interactive prompts. |
| `stdout` | `fn stdout() -> io::Writer` | Returns a handle to the process's standard output stream. |
| `stderr` | `fn stderr() -> io::Writer` | Returns a handle to the process's standard error stream. |
| `ReadAll` | `fn ReadAll(reader: io::Reader) -> Result<String, io::Error>` | Drains a reader to a String. Mirrors Go's io.ReadAll. |
| `Copy` | `fn Copy(dst: io::Writer, src: io::Reader) -> Result<i64, io::Error>` | Copies all bytes from src to dst; returns the byte count. |
| `Error` | `type Error` | Errors raised by I/O operations. |
| `MemoryStream` | `type MemoryStream` | An in-memory reader or writer made by the functions below. Copies of a stream name the same stream, and adapters built on it keep it alive; it is freed with the last of them. |
| `string_reader` | `fn string_reader(text: String) -> io::MemoryStream` | `string_reader(text: String) -> io::MemoryStream` - a reader over an in-memory buffer. The adapters below compose streams by value. Example: `let src = io::string_reader("hello")`. |
| `buffer_writer` | `fn buffer_writer() -> io::MemoryStream` | `buffer_writer() -> io::MemoryStream` - a writer collecting everything written to it; read it back with `io::contents`. |
| `limit_reader` | `fn limit_reader(src: io::MemoryStream, limit: i64) -> io::MemoryStream` | `limit_reader(src: io::MemoryStream, limit: i64) -> io::MemoryStream` - a reader yielding at most `limit` bytes from `src`, Go's `io.LimitReader`. Example: `io::drain(io::limit_reader(src, 5))`. |
| `tee_reader` | `fn tee_reader(src: io::MemoryStream, sink: io::MemoryStream) -> io::MemoryStream` | `tee_reader(src: io::MemoryStream, sink: io::MemoryStream) -> io::MemoryStream` - a reader mirroring every byte read from `src` into the writer `sink`, Go's `io.TeeReader`. |
| `multi_reader` | `fn multi_reader(sources: Vec<io::MemoryStream>) -> io::MemoryStream` | `multi_reader(sources: Vec<io::MemoryStream>) -> io::MemoryStream` - a reader draining each source in turn, Go's `io.MultiReader`. Example: `io::multi_reader(#[a, b])`. |
| `pipe` | `fn pipe() -> (io::MemoryStream, io::MemoryStream)` | `pipe() -> (io::MemoryStream, io::MemoryStream)` - a connected `(reader, writer)` pair sharing one in-memory buffer. Reads return the bytes written so far and never block; `io::close_writer` marks the writer done. Example: `let r, w = io::pipe()`. |
| `copy_n` | `fn copy_n(dst: io::MemoryStream, src: io::MemoryStream, n: i64) -> Result<i64, errors::Error>` | `copy_n(dst: io::MemoryStream, src: io::MemoryStream, n: i64) -> Result<i64, errors::Error>` - copies at most `n` bytes and returns the count actually transferred. Go's `io.CopyN`. |
| `drain` | `fn drain(src: io::MemoryStream) -> String` | `drain(src: io::MemoryStream) -> String` - reads a reader to the end of stream as UTF-8 text. |
| `contents` | `fn contents(writer: io::MemoryStream) -> String` | `contents(writer: io::MemoryStream) -> String` - everything written to a buffer or pipe writer, as UTF-8 text. |
| `write` | `fn write(writer: io::MemoryStream, text: String) -> i64` | `write(writer: io::MemoryStream, text: String) -> i64` - appends text to a writer and returns the byte count accepted. |
| `close_writer` | `fn close_writer(writer: io::MemoryStream)` | `close_writer(writer: io::MemoryStream)` - signals end of stream on a pipe writer; later writes are rejected. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## Buffering

Standard output is buffered so that the several writes a formatted line
arrives as - the literal segments, a rendered value, the newline - cost one
`write(2)`. The buffer drains on the newline that ends a write, so a line is
on its way out as soon as it is complete, whether standard output is a
terminal, a pipe, or a file. A program that announces a line and then blocks
is therefore visible to whatever is reading it:

<!-- fragment -->
```gossamer
println("{}", server.addr())
server.serve(routes)?
```

Text with no terminator accumulates: a prompt written with `print` needs an
explicit flush before the read.

```gossamer
use std::io
print("name: ")
io::stdout().flush()
```

Byte-at-a-time and byte-range writes accumulate too - that is the
high-throughput path, and it drains when the buffer fills, on an explicit
`flush`, and at exit. Standard error is never buffered, and writing to it
flushes standard output first, so the two streams keep their order.
