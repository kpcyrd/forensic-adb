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
