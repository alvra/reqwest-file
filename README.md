# Reqwest-File

Use web resources like regular async files.

## Features

  * No unsafe code (`#[forbid(unsafe_code)]`)
  * Tested; code coverage: 100%

## Example

```rust
use reqwest_file::RequestFile;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

let client = reqwest::Client::new();
let request = client.get("http://httpbin.org/base64/aGVsbG8gd29ybGQ=");
let mut file: RequestFile = RequestFile::new(request);

let mut buffer = [0; 5];
assert_eq!(file.read(&mut buffer).await.unwrap(), 5);
assert_eq!(&buffer, b"hello");

let mut buffer = [0; 5];
assert_eq!(file.seek(std::io::SeekFrom::Current(1)).await.unwrap(), 6);
assert_eq!(file.read(&mut buffer).await.unwrap(), 5);
assert_eq!(&buffer, b"world");
```

## Documentation

[Documentation](https://lib.rs/crates/reqwest-file)
