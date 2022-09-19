# Reqwest-File

[![Build status](https://github.com/alvra/reqwest-file/actions/workflows/check.yml/badge.svg?branch=main)](https://github.com/alvra/reqwest-file/actions/workflows/check.yml)
[![Crates.io](https://img.shields.io/crates/v/reqwest-file)](https://crates.io/crates/reqwest-file)
[![Documentation](https://docs.rs/reqwest-file/badge.svg)](https://docs.rs/reqwest-file)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](https://github.com/rust-secure-code/safety-dance/)

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

## License

Licensed under either of

 * Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license
   ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
