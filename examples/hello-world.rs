use bstr::ByteSlice;
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

    let output = device.execute_host_exec_out_command("id").await?;
    println!("Received response: {:?}", bstr::BStr::new(&output));

    let output = device
        .execute_host_exec_out_command("pm list packages -f")
        .await?;
    for line in output.lines() {
        println!("Received line: {:?}", bstr::BStr::new(line));
    }

    Ok(())
}
