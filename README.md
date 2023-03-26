# forensic-adb

Tokio based client library for the Android Debug Bridge (adb) based on mozdevice for Rust.

[Documentation](https://docs.rs/forensic-adb)

This code has been extracted from [mozilla-central/testing/mozbase/rust/mozdevice][1] and ported to async Rust. It also removes root detection so no commands are executed on the remote device by default.

[1]: https://hg.mozilla.org/mozilla-central/file/tip/testing/mozbase/rust/mozdevice

```rust
use forensic_adb::{AndroidStorageInput, DeviceError, Host};

#[tokio::main]
async fn main() -> Result<(), DeviceError> {
    let host = Host::default();

    let devices = host.devices::<Vec<_>>().await?;
    println!("Found devices: {:?}", devices);

    let device = host
        .device_or_default(Option::<&String>::None, AndroidStorageInput::default())
        .await?;
    println!("Selected device: {:?}", device);

    let output = device.execute_host_shell_command("id").await?;
    println!("Received response: {:?}", output);

    Ok(())
}
```

## License

Mozilla Public License (MPL-2.0)
