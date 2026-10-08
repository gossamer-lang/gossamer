# `std::fs`

Filesystem reading, writing, and traversal (Rust std::fs shape).

## Items

| Item | Signature | Description |
|---|---|---|
| `File` | `type File` | Streaming file handle. Reads and writes at the handle's own cursor (read, read_to_string, write, write_bytes, seek), positionally (read_at, read_at_into, write_at), and reports size (len, set_len). Durability is sync_all / sync_data; multi-process safety is the try_lock_* / unlock family. The file closes at `close()` or with its last handle. `std::term` and `os::fd` calls take the file itself, which keeps it open while they use it; `fd()` answers the OS descriptor (a handle on Windows) for a foreign call. |
| `DirInfo` | `type DirInfo` | Directory entry yielded by read_dir and walk_dir; carries path, name, is_file, is_dir, is_symlink, and size. |
| `OpenOptions` | `type OpenOptions` | Builder for opening files with read/write/append/create/truncate flags. |
| `open` | `fn open(path: String) -> Result<fs::File, io::Error>` | Opens a file for streaming reads. |
| `create` | `fn create(path: String) -> Result<fs::File, io::Error>` | Creates or truncates a file and returns a streaming file handle. |
| `temp_dir` | `fn temp_dir(prefix: String) -> Result<String, io::Error>` | Creates a unique temporary directory; the caller removes it explicitly. |
| `temp_file` | `fn temp_file(prefix: String) -> Result<(fs::File, String), io::Error>` | Creates a unique temporary file and returns its handle plus path. |
| `read` | `fn read(path: String) -> Result<Vec<u8>, io::Error>` | Reads an entire file into memory as bytes. |
| `read_to_string` | `fn read_to_string(path: String) -> Result<String, io::Error>` | Reads an entire file into memory as UTF-8 text. |
| `write` | `fn write(path: String, contents: Vec<u8>) -> Result<(), io::Error>` | Writes bytes to a file, creating or truncating it. |
| `read_dir` | `fn read_dir(path: String) -> Result<Vec<fs::DirInfo>, io::Error>` | Returns immediate children as DirInfo values. Inspect their metadata fields directly; each path can be passed back to filesystem APIs. |
| `walk_dir` | `fn walk_dir(path: String, visit: Fn(fs::DirInfo) -> Result<(), io::Error>) -> Result<(), io::Error>` | Recursively visits every descendant entry. |
| `create_dir` | `fn create_dir(path: String) -> Result<(), io::Error>` | Creates a single directory. Fails if any parent is missing. |
| `create_dir_all` | `fn create_dir_all(path: String) -> Result<(), io::Error>` | Creates a directory and any missing ancestors. |
| `create_dir_mode` | `fn create_dir_mode(path: String, mode: i64) -> Result<(), io::Error>` | Creates a single directory with exactly this mode, whatever the umask is. On Windows only the owner write bit is meaningful: it sets the read-only attribute. |
| `create_dir_all_mode` | `fn create_dir_all_mode(path: String, mode: i64) -> Result<(), io::Error>` | Creates a directory and any missing ancestors, giving each one it creates exactly this mode. |
| `write_mode` | `fn write_mode(path: String, contents: Vec<u8>, mode: i64) -> Result<(), io::Error>` | Writes a file and leaves it at exactly this mode, whatever the umask is. |
| `permissions` | `fn permissions(path: String) -> Result<i64, io::Error>` | The permission bits of a path, in the chmod(2) encoding. On Windows the read-only attribute is widened into the bits an equivalent Unix path would carry. |
| `set_permissions` | `fn set_permissions(path: String, mode: i64) -> Result<(), io::Error>` | Sets the permission bits of a path, in the chmod(2) encoding. On Windows only the owner write bit is meaningful: it sets or clears the read-only attribute. |
| `remove_file` | `fn remove_file(path: String) -> Result<(), io::Error>` | Removes a single file. |
| `remove_dir` | `fn remove_dir(path: String) -> Result<(), io::Error>` | Removes an empty directory. |
| `remove_dir_all` | `fn remove_dir_all(path: String) -> Result<(), io::Error>` | Recursively removes a directory and its contents. |
| `copy` | `fn copy(src: String, dst: String) -> Result<i64, io::Error>` | Copies a file, creating parent dirs as needed. |
| `rename` | `fn rename(src: String, dst: String) -> Result<(), io::Error>` | Renames a file or directory. |
| `exists` | `fn exists(path: String) -> bool` | Returns whether a path exists on the filesystem. |
| `is_file` | `fn is_file(path: String) -> bool` | Returns whether a path exists and is a regular file. |
| `is_dir` | `fn is_dir(path: String) -> bool` | Returns whether a path exists and is a directory. |
| `is_symlink` | `fn is_symlink(path: String) -> bool` | Returns whether a path exists and is a symbolic link. |
| `file_size` | `fn file_size(path: String) -> i64` | Returns the file's size in bytes; 0 on error. |
| `metadata` | `fn metadata(path: String) -> Result<fs::Metadata, io::Error>` | Returns filesystem metadata for a path. |
| `sync_dir` | `fn sync_dir(path: String) -> Result<(), io::Error>` | Makes a directory's own entries durable - the barrier a create, rename, or delete needs after the file itself is synced. On Windows this is satisfied by NTFS metadata ordering and performs no flush. |
| `SEEK_SET` | `const SEEK_SET` | File::seek whence: the offset is absolute from the start of the file. |
| `SEEK_CUR` | `const SEEK_CUR` | File::seek whence: the offset is relative to the current position. |
| `SEEK_END` | `const SEEK_END` | File::seek whence: the offset is relative to the end of the file. |
| `canonicalize` | `fn canonicalize(path: String) -> Result<String, io::Error>` | Resolves a path to an absolute, symlink-free canonical form. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
